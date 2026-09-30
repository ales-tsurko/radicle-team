//! Sandbox enforcement observed by spawning real processes.
//!
//! This target sets `harness = false` because Linux sandboxing re-enters the running executable as
//! `zerobox-linux-sandbox`, and that re-entry must be dispatched before argument parsing. A libtest
//! binary's `main` belongs to the harness and never dispatches, so the helper cannot start and no
//! sandboxed child launches. Owning `main` here keeps the same checks running on macOS and Linux.

use std::{
    fs,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    process::ExitStatus,
};

use radicle_team::{
    agent::McpEndpoint,
    home::MemberHome,
    sandbox::{AgentSandbox, Config, NetworkAccess},
};
use url::Url;

type Check = fn() -> Pin<Box<dyn Future<Output = ()>>>;

const CHECKS: &[(&str, Check)] = &[
    ("confines_the_file_system", || {
        Box::pin(confines_the_file_system())
    }),
    ("restricts_loopback_per_platform", || {
        Box::pin(restricts_loopback_per_platform())
    }),
    (
        "blocks_external_hosts",
        || Box::pin(blocks_external_hosts()),
    ),
    ("reloads_configuration_between_commands", || {
        Box::pin(reloads_configuration_between_commands())
    }),
    ("reaches_the_mcp_endpoint_with_networking_disabled", || {
        Box::pin(reaches_the_mcp_endpoint_with_networking_disabled())
    }),
    ("guards_the_shared_git_store", || {
        Box::pin(guards_the_shared_git_store())
    }),
];

fn main() {
    radicle_team::sandbox::dispatch_linux_helper();

    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    // `--list` is the libtest protocol that `cargo nextest` uses to enumerate a binary's tests. It
    // also lists with `--ignored` to learn which of them to skip; none of these are ignored, so
    // that combination must report nothing rather than repeating the whole set.
    if arguments.iter().any(|argument| argument == "--list") {
        if !arguments.iter().any(|argument| argument == "--ignored") {
            for (name, _) in CHECKS {
                println!("{name}: test");
            }
        }
        return;
    }
    let selected = arguments
        .iter()
        .find(|argument| !argument.starts_with('-'))
        .map(String::as_str);

    // Agent sandboxing fails closed where Zerobox has no enforcing backend, so there is nothing to
    // observe on those platforms.
    if !cfg!(any(target_os = "macos", target_os = "linux")) {
        println!(
            "sandbox_spawn: SKIPPED on {}, agent sandboxing has no enforcing backend there",
            std::env::consts::OS
        );
        return;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("test runtime should build");
    for (name, check) in CHECKS
        .iter()
        .filter(|(name, _)| selected.is_none_or(|selected| selected == *name))
    {
        runtime.block_on(check());
        println!("test {name} ... ok");
    }
}

/// Proves a denied path is unreadable *because of the policy*.
///
/// The secret is read successfully before the deny is applied, so a later failure cannot be a bad
/// path or a broken sandbox that fails every command.
async fn confines_the_file_system() {
    let fixture = Fixture::new("file-system");
    let permitted = fixture.workspaces().join("permitted.txt");
    fs::write(&permitted, b"permitted").expect("permitted file should be written");
    let secrets = fixture.root().join("secrets");
    fs::create_dir_all(&secrets).expect("secret directory should be created");
    let secret = secrets.join("secret.txt");
    fs::write(&secret, b"secret").expect("secret file should be written");

    let sandbox = fixture.sandbox(Config::default());
    check(
        read(&sandbox, &permitted).await.success(),
        "a workspace file must stay readable; without this control a denial proves nothing",
    );
    check(
        read(&sandbox, &secret).await.success(),
        "the secret must be readable before it is denied; without this control the denial below \
         could come from a bad path rather than the policy",
    );

    let mut config = Config::default();
    config.set_deny_read(vec![secrets]);
    let sandbox = fixture.sandbox(config);
    check(
        !read(&sandbox, &secret).await.success(),
        "a denied file must be unreadable inside the sandbox",
    );
}

/// Records how each platform treats loopback once networking is disabled.
///
/// macOS keeps host loopback reachable, so an agent can use a local development or language
/// server directly. Linux confines the process to its own network namespace, so the same address
/// is unreachable and only the proxy bridges out. Both are asserted because the difference decides
/// whether a local service is isolated from the agent.
async fn restricts_loopback_per_platform() {
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("local listener should bind");
    let port = listener
        .local_addr()
        .expect("local address should be available")
        .port();
    let _server = tokio::spawn(async move {
        while let Ok((stream, _peer)) = listener.accept().await {
            tokio::spawn(serve(stream));
        }
    });
    let fixture = Fixture::new("loopback");
    let target = format!("http://127.0.0.1:{port}/");

    let mut config = Config::default();
    config.set_network(NetworkAccess::All);
    check(
        fetch(&fixture.sandbox(config), &target).await.success(),
        "an unrestricted sandbox must reach the listener; without this control the result below \
         could be a missing curl or a dead listener rather than the policy",
    );

    let mut config = Config::default();
    config.set_network(NetworkAccess::Disabled);
    let reachable = fetch(&fixture.sandbox(config), &target).await.success();
    if cfg!(target_os = "macos") {
        check(
            reachable,
            "macOS must keep host loopback reachable with networking disabled, so a local service \
             is not isolated from the agent",
        );
    } else {
        check(
            !reachable,
            "Linux runs the sandbox in its own network namespace, so host loopback must be \
             unreachable with networking disabled",
        );
    }
}

/// Proves external traffic is stopped by the sandbox rather than only by the proxy.
///
/// The probe ignores proxy environment variables, so reaching the host would mean the sandbox
/// itself permits direct external traffic.
async fn blocks_external_hosts() {
    const EXTERNAL: &str = "http://example.com/";

    let fixture = Fixture::new("external");
    let mut config = Config::default();
    config.set_network(NetworkAccess::All);
    if !fetch(&fixture.sandbox(config), EXTERNAL).await.success() {
        println!(
            "sandbox_spawn: SKIPPED blocks_external_hosts, no connectivity to {EXTERNAL}, so a \
             blocked result would prove nothing"
        );
        return;
    }

    let mut config = Config::default();
    config.set_network(NetworkAccess::Disabled);
    check(
        !fetch(&fixture.sandbox(config), EXTERNAL).await.success(),
        "a sandbox with networking disabled must not reach an external host",
    );
}

/// Proves an operator can lift a denial without restarting the member.
///
/// The same sandbox is reused across both reads, so a change in outcome can only come from the
/// configuration being reloaded for each spawn.
async fn reloads_configuration_between_commands() {
    let fixture = Fixture::new("reload");
    let secrets = fixture.root().join("secrets");
    fs::create_dir_all(&secrets).expect("secret directory should be created");
    let secret = secrets.join("secret.txt");
    fs::write(&secret, b"secret").expect("secret file should be written");

    let mut config = Config::default();
    config.set_deny_read(vec![secrets]);
    let sandbox = fixture.sandbox(config);
    check(
        !read(&sandbox, &secret).await.success(),
        "the denied file must start out unreadable",
    );

    fixture.write_config(&Config::default());

    check(
        read(&sandbox, &secret).await.success(),
        "widening the configuration must take effect on the next command, without reloading the \
         sandbox itself",
    );
}

/// Proves an agent can still reach its MCP endpoint once networking is disabled.
///
/// This is the one loopback address a restricted sandbox must keep usable, and the platforms get
/// there differently: macOS permits host loopback directly, while Linux runs the process in its
/// own network namespace and bridges the endpoint through the managed proxy. The probe therefore
/// honours the proxy environment, as an MCP client must.
async fn reaches_the_mcp_endpoint_with_networking_disabled() {
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("MCP listener should bind");
    let port = listener
        .local_addr()
        .expect("MCP address should be available")
        .port();
    let _server = tokio::spawn(async move {
        while let Ok((stream, _peer)) = listener.accept().await {
            tokio::spawn(serve(stream));
        }
    });
    let fixture = Fixture::new("mcp");
    let endpoint = Url::parse(&format!("http://127.0.0.1:{port}/mcp/capability"))
        .expect("MCP endpoint should be valid");

    let mut config = Config::default();
    config.set_network(NetworkAccess::Disabled);
    let sandbox = fixture.sandbox_for(config, &McpEndpoint::new(endpoint.clone()));

    check(
        proxied(&sandbox, endpoint.as_str()).await.success(),
        "the MCP endpoint must stay reachable with networking disabled, or an agent cannot use \
         any tool once the sandbox is restricted",
    );
}

// Proves the shared Git store's carveouts against a real sandbox rather than the policy alone.
//
// The runtime runs Git against this store outside the sandbox, so anything a role can write there
// which Git later reads is a way of choosing what the runtime executes. The commit at the end is
// the other half: the store has to stay writable enough to be usable at all.
async fn guards_the_shared_git_store() {
    let fixture = Fixture::new("git-store");
    let common = fixture.git_store();
    let sandbox = fixture.sandbox(Config::default());

    check(
        write(&sandbox, &common.join("objects").join("probe"))
            .await
            .success(),
        "the object store must stay writable; without this control a denial proves nothing",
    );
    for denied in [
        common.join("config"),
        common.join("hooks").join("post-checkout"),
        common.join("packed-refs"),
        common.join("refs").join("heads").join("main"),
        common.join("worktrees").join("coder").join("commondir"),
        common.join("worktrees").join("talker").join("index"),
        fixture.workspaces().join(".git"),
    ] {
        check(
            !write(&sandbox, &denied).await.success(),
            &format!(
                "{} must not be writable inside the sandbox",
                denied.display()
            ),
        );
    }

    let worktree = fixture.workspaces();
    check(
        run(sandbox
            .command("/bin/sh")
            .arg("-c")
            .arg("echo agent > written.txt && git add written.txt && git commit -q -m Agent"))
        .await
        .success(),
        "the building role must still be able to commit in its own working copy",
    );
    check(
        std::process::Command::new("git")
            .args(["rev-parse", "--verify", "refs/heads/nico-rt/job-1"])
            .current_dir(&worktree)
            .status()
            .expect("git should run")
            .success(),
        "the commit the role made must be on its branch in the shared store",
    );
}

async fn write(sandbox: &AgentSandbox, path: &Path) -> ExitStatus {
    run(sandbox
        .command("/bin/sh")
        .arg("-c")
        .arg(format!("echo probe >> '{}'", path.display())))
    .await
}

struct Fixture {
    directory: tempfile::TempDir,
    home: MemberHome,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let directory = tempfile::Builder::new()
            .prefix(name)
            .tempdir()
            .expect("temporary directory should be created");
        let home = MemberHome::at(directory.path());

        Self { directory, home }
    }

    fn root(&self) -> &Path {
        self.directory.path()
    }

    fn workspaces(&self) -> PathBuf {
        let workspaces = self.home.worktree(radicle_team::agent::Role::Coder);
        fs::create_dir_all(&workspaces).expect("workspaces should be created");
        workspaces
    }

    // Builds the shared object store and the working copies the way the runtime does.
    fn git_store(&self) -> PathBuf {
        let source = self.root().join("source");
        fs::create_dir_all(&source).expect("source should be created");
        git(&source, &["init", "-q", "."]);
        // Named here rather than taken from the machine, which need not have a Git identity at all.
        git(&source, &["config", "user.name", "Sandbox Test"]);
        git(&source, &["config", "user.email", "sandbox@example.com"]);
        fs::write(source.join("README.md"), "source\n").expect("README should be written");
        git(&source, &["add", "--all"]);
        git(&source, &["commit", "-q", "-m", "Initial"]);

        let common = self.home.git_common();
        fs::create_dir_all(common.parent().expect("the store has a parent"))
            .expect("code directory should be created");
        git(
            self.root(),
            &[
                "clone",
                "-q",
                "--bare",
                &source.display().to_string(),
                &common.display().to_string(),
            ],
        );
        git(&common, &["config", "user.name", "Sandbox Test"]);
        git(&common, &["config", "user.email", "sandbox@example.com"]);
        fs::create_dir_all(common.join("refs").join("heads").join("nico-rt"))
            .expect("branch directory should be created");
        for role in [
            radicle_team::agent::Role::Talker,
            radicle_team::agent::Role::Chatter,
            radicle_team::agent::Role::Coder,
        ] {
            let worktree = self.home.worktree(role);
            git(
                &common,
                &[
                    "worktree",
                    "add",
                    "--relative-paths",
                    "--detach",
                    &worktree.display().to_string(),
                    "HEAD",
                ],
            );
        }
        git(
            &self.home.worktree(radicle_team::agent::Role::Coder),
            &["checkout", "-q", "-B", "nico-rt/job-1"],
        );

        common
    }

    // Writes `config` as the member's sandbox configuration and loads a sandbox using it.
    fn sandbox(&self, config: Config) -> AgentSandbox {
        self.sandbox_for(config, &endpoint())
    }

    fn sandbox_for(&self, config: Config, endpoint: &McpEndpoint) -> AgentSandbox {
        self.write_config(&config);

        AgentSandbox::load(
            &self.home,
            radicle_team::member::Member::Nico,
            radicle_team::agent::Role::Coder,
            endpoint,
        )
        .expect("sandbox should load")
    }

    fn write_config(&self, config: &Config) {
        let path = self.home.sandbox_config();
        let _removed = fs::remove_file(&path);
        config
            .write(&path)
            .expect("sandbox configuration should be written");
    }
}

fn git(directory: &Path, arguments: &[&str]) {
    let status = std::process::Command::new("git")
        .args(arguments)
        .current_dir(directory)
        .status()
        .expect("git should run");
    assert!(status.success(), "git {arguments:?} failed");
}

fn endpoint() -> McpEndpoint {
    McpEndpoint::new(
        Url::parse("http://127.0.0.1:12345/mcp/capability").expect("MCP endpoint should be valid"),
    )
}

/// Answers one request, reading it first so the peer never sees a reset instead of the response.
async fn serve(mut stream: tokio::net::TcpStream) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let mut request = [0_u8; 1024];
    let _read = stream.read(&mut request).await;
    let _written = stream
        .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
        .await;
    let _flushed = stream.flush().await;
}

/// Fetches `target` while honouring the proxy environment the sandbox injects.
async fn proxied(sandbox: &AgentSandbox, target: &str) -> ExitStatus {
    run(sandbox
        .command("curl")
        .args(["-s", "--fail", "-o", "/dev/null", "-m", "10", target]))
    .await
}

async fn read(sandbox: &AgentSandbox, path: &Path) -> ExitStatus {
    run(sandbox.command("/bin/cat").arg(path.display().to_string())).await
}

async fn fetch(sandbox: &AgentSandbox, target: &str) -> ExitStatus {
    run(sandbox.command("curl").args([
        "-s",
        "--noproxy",
        "*",
        "--fail",
        "-o",
        "/dev/null",
        "-m",
        "10",
        target,
    ]))
    .await
}

/// Runs `command` and reports the child's own diagnostics when it fails.
///
/// A bare exit status cannot distinguish "the policy denied this" from "the sandbox could not
/// start", so the child's stderr is surfaced whenever it does not succeed.
async fn run(command: radicle_team::sandbox::AgentCommand) -> ExitStatus {
    let mut child = command
        .spawn()
        .await
        .expect("sandboxed process should spawn");
    let errors = child.stderr();
    let status = child.wait().await.expect("sandboxed process should exit");
    if !status.success() {
        let mut message = String::new();
        if let Some(mut errors) = errors {
            let _read = tokio::io::AsyncReadExt::read_to_string(&mut errors, &mut message).await;
        }
        // Written to stderr so it is unbuffered and ordered against the panic that follows; a
        // piped stdout is block buffered and could surface after the failure, or not at all.
        eprintln!(
            "sandbox_spawn: child failed with {status}; stderr: {}",
            message.trim()
        );
    }

    status
}

fn check(passed: bool, message: &str) {
    assert!(passed, "{message}");
}
