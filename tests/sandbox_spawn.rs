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
    member::Member,
    sandbox::{AgentSandbox, Config, ExternalNetwork},
};
use url::Url;

type Check = fn() -> Pin<Box<dyn Future<Output = ()>>>;

const CHECKS: &[(&str, Check)] = &[
    ("confines_the_file_system", || {
        Box::pin(confines_the_file_system())
    }),
    ("leaves_loopback_reachable", || {
        Box::pin(leaves_loopback_reachable())
    }),
    (
        "blocks_external_hosts",
        || Box::pin(blocks_external_hosts()),
    ),
    ("reloads_configuration_between_commands", || {
        Box::pin(reloads_configuration_between_commands())
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

    // Miri implements neither the polling syscalls a Tokio IO runtime needs nor process spawning,
    // so these checks cannot run under it.
    if cfg!(miri) {
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

/// Records that loopback stays reachable even with external networking disabled.
///
/// Agents bind and reach local ports for development and language servers, so the sandbox permits
/// loopback directly rather than through the proxy. A failure here means that contract changed.
async fn leaves_loopback_reachable() {
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("local listener should bind");
    let port = listener
        .local_addr()
        .expect("local address should be available")
        .port();
    let _server = tokio::spawn(async move {
        while let Ok((mut stream, _peer)) = listener.accept().await {
            use tokio::io::AsyncWriteExt as _;
            let _written = stream
                .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                .await;
        }
    });
    let fixture = Fixture::new("loopback");
    let target = format!("http://127.0.0.1:{port}/");

    let mut config = Config::default();
    config.set_external_network(ExternalNetwork::All);
    check(
        fetch(&fixture.sandbox(config), &target).await.success(),
        "an unrestricted sandbox must reach the listener; without this control the result below \
         could be a missing curl or a dead listener rather than the policy",
    );

    let mut config = Config::default();
    config.set_external_network(ExternalNetwork::Disabled);
    check(
        fetch(&fixture.sandbox(config), &target).await.success(),
        "loopback must stay reachable when external networking is disabled",
    );
}

/// Proves external traffic is stopped by the sandbox rather than only by the proxy.
///
/// The probe ignores proxy environment variables, so reaching the host would mean the sandbox
/// itself permits direct external traffic.
async fn blocks_external_hosts() {
    const EXTERNAL: &str = "http://example.com/";

    let fixture = Fixture::new("external");
    let mut config = Config::default();
    config.set_external_network(ExternalNetwork::All);
    if !fetch(&fixture.sandbox(config), EXTERNAL).await.success() {
        println!(
            "sandbox_spawn: SKIPPED blocks_external_hosts, no connectivity to {EXTERNAL}, so a \
             blocked result would prove nothing"
        );
        return;
    }

    let mut config = Config::default();
    config.set_external_network(ExternalNetwork::Disabled);
    check(
        !fetch(&fixture.sandbox(config), EXTERNAL).await.success(),
        "a sandbox with external networking disabled must not reach an external host",
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
        let home = MemberHome::at(directory.path(), Member::Nico);

        Self { directory, home }
    }

    fn root(&self) -> &Path {
        self.directory.path()
    }

    fn workspaces(&self) -> PathBuf {
        let workspaces = self.home.workspaces();
        fs::create_dir_all(&workspaces).expect("workspaces should be created");
        workspaces
    }

    /// Writes `config` as the member's sandbox configuration and loads a sandbox using it.
    fn sandbox(&self, config: Config) -> AgentSandbox {
        self.write_config(&config);

        AgentSandbox::load(&self.home, &endpoint()).expect("sandbox should load")
    }

    fn write_config(&self, config: &Config) {
        let path = self.home.sandbox_config();
        let _removed = fs::remove_file(&path);
        config
            .write(&path)
            .expect("sandbox configuration should be written");
    }
}

fn endpoint() -> McpEndpoint {
    McpEndpoint::new(
        Url::parse("http://127.0.0.1:12345/mcp/capability").expect("MCP endpoint should be valid"),
    )
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

async fn run(command: radicle_team::sandbox::AgentCommand) -> ExitStatus {
    command
        .spawn()
        .await
        .expect("sandboxed process should spawn")
        .wait()
        .await
        .expect("sandboxed process should exit")
}

fn check(passed: bool, message: &str) {
    assert!(passed, "{message}");
}
