# Local sandbox

Setup creates an administrator-owned sandbox configuration for each member:

```text
<member folder>/sandbox.toml
```

The interactive wizard optionally adds readable, writable, and denied filesystem
paths and selects the network policy. The file can also be edited manually.

This file is reloaded for every sandboxed command, so widening a denied path
takes effect on the member's next command without a restart. Commands already
running keep the policy they started with. An edit that leaves the file unsafe
or invalid fails the next spawn rather than silently reusing the previous
policy, so a typo surfaces immediately instead of appearing to apply.

[The repository manifest](manifest.md) deliberately does **not** work this way:
it is read once at startup. The asymmetry is intentional. A sandbox denial
interrupts work already in progress, and the operator fixing it wants the
running member to recover. Manifest values block nothing, and reloading only
some of them would leave a member acting on two different revisions of the same
file at once.

Agent adapters select commands and arguments but cannot weaken this policy.
Every subprocess inherits the same sandbox.

So does the runtime's own Git. It is the more trusted side, but it runs Git
against working copies an agent can write, and Git decides what to do from what
it finds in the repository it is pointed at. Fencing off each way an agent could
reach that decision is guesswork, and one missed way would be a way out of the
sandbox altogether. The runtime's Git therefore runs in a sandbox of its own.
The working copy the command is preparing is writable, along with the shared
object store. The other roles' working copies are not, neither their files nor
the entries the store keeps for them, so a command run for one role reaches no
further into another than that role's agent could itself.

The one repository in Radicle storage a delivery is fetched from is readable,
and the rest of storage is not.

The member's identity is out of reach and external network access is denied.
`sandbox.toml` does not apply either: widening what an agent may reach must not
widen what code smuggled into a Git command may reach.

Denied networking still leaves host loopback reachable on macOS, the same
platform difference described under [Network](#network) below. A local service
is therefore not isolated from a runtime Git command any more than it is from an
agent.




## Filesystem paths

Radicle Team loads Zerobox's embedded platform and sensitive-path policies
directly, so user-defined Zerobox profiles cannot replace them. It also enables
common Rust, Node.js, Python, Go, and Nix toolchain paths. It grants write
access to the one working copy belonging to the role being run, that role's own
`harness/` directory, shared native provider state in `harness/.providers/`, the
member's `cache/` directory, and the shared object store in `.git-common`,
without which the agent could edit files but never commit them. Each role has
its own checkout and configuration.

Native provider state gives all roles one sign-in. `cache/` also stays shared:
the member's three roles share it, because build output is far too large to keep
three copies of. Sharing it there costs nothing. The separation between one
member's roles is about not standing on each other's work — a checked-out
branch, a build lock, a transcript — and never about holding one of them away
from the others. They are one person under one key, and the repository's own
build scripts already run inside the building agent's sandbox every time it
builds.

It never reaches further than the member, and that boundary is real. A build
cache holds compiled build scripts and plugins which the next build runs, so two
members sharing one would let either run code as the other.

Parts of that store stay read-only, because Git executes whatever its
configuration and hooks name, and the runtime's own Git reads them too:

- `.git-common/config` and `.git-common/hooks`, which say what Git runs.
- `<role>/.git` and `.git-common/worktrees/<role>/{commondir,gitdir}`, which say
  where Git looks. Repointed at a store the agent controls, the runtime's next
  Git command would read that store's configuration and hooks.
- `.git-common/refs` and `.git-common/packed-refs`, so no role can move or
  delete branches belonging to another. The building role is granted
  `refs/heads/<member>-rt` back, since that is where its own branches live. The
  talking and chatting roles answer from a detached head and need none. Anything
  writing a reference outside that namespace fails inside the sandbox, including
  `git stash`, `git tag`, and branches of the agent's own choosing.
- Every other role's directory under `.git-common/worktrees`.

Fencing paths off only covers what the runtime *reads*. The runtime also writes
references there, and a name the building agent can write may be made to stand
for another reference entirely. So every reference the runtime writes is one it
named itself, addressed in full and set without following it. An agent can do
what it likes inside `<member>-rt/`; it cannot reach outside it.

Neither of those is the boundary. They narrow what an agent can put in a Git
command's way; the sandbox above is what bounds the cost of one that gets
through anyway.

Two things follow from the store's configuration being read-only. The member's
Git name and email are written there at startup, because an agent cannot set
them and Git would otherwise stop at the first commit to ask who is making it.
Automatic housekeeping is turned off, because it would try to rewrite
`packed-refs`; run `git gc` in `.git-common` yourself if the store grows.

Sandboxed commands also run with `GIT_CONFIG_GLOBAL` and `GIT_CONFIG_NOSYSTEM`
set to Git's defaults. The operating-system user's Git configuration is denied
by the policy, and Git refuses to run at all when it cannot read a file it was
told to read.

Local path entries extend or restrict those defaults, and denied paths take
precedence. Paths in the file must be absolute.

```toml
version = 1
allow_read = ["/custom/toolchain"]
deny_read = ["/private/project"]
allow_write = ["/custom/build-cache"]
deny_write = ["/custom/build-cache/protected"]
network = "all"
```




## Network

Network access can be unrestricted, disabled, or limited to domains:

```toml
network = "all"                                          # default when omitted
network = "disabled"
network = ["api.openai.com", "crates.io", "github.com"]
```

A restricted list has to cover the agent application as well as the provider's
own API: agent applications fetch model registries and their own dependencies,
and a list without those leaves a member unable to start. What each one needs is
in [adapters and providers](adapters/index.md).

Disabled and domain-restricted modes route external traffic through the managed
proxy, which accepts only the capability-protected MCP endpoint.




## Loopback

Loopback behaves differently on each platform, because Zerobox uses a different
backend on each, and neither behaviour is configurable from here. The member's
own MCP endpoint stays reachable under every setting, directly on macOS and
through the managed proxy on Linux, which bridges it into the namespace.

On **macOS** the sandbox keeps host loopback reachable under every setting. An
agent can use a local development or language server directly, and a local
service is **not** isolated from it. Do not rely on this setting to isolate
something sensitive: bind it elsewhere, or require authentication.

On **Linux** a restricted sandbox runs in its own network namespace, so host
loopback is unreachable and only the managed proxy bridges out. Zerobox creates
that namespace per command, so a server one command starts is not reachable from
the next one. A workflow that runs a local development server and then connects
to it therefore needs `network = "all"` on Linux. Domains do not help, because
that is still a restricted sandbox. A restricted sandbox on Linux also blocks
the command from creating new Unix domain sockets, so local services reached
that way, such as a container daemon or a database socket, are unavailable there
as well.

If you need both an isolated internet and a working local server on Linux, run
the whole member inside a container with `network = "all"` and filter egress at
the container instead. Leave the container able to reach your Radicle seeds, or
`radicle-node` cannot synchronize.




## Platform support

Linux and macOS only. Zerobox provides an enforcing backend on those two and
nowhere else, so the crate refuses to compile anywhere else rather than
producing a binary which runs agents unconfined. An agent is given unrestricted
permissions on purpose — there is no way for it to stop and ask a person before
acting, so nothing would ever be confirmed and it would stall. The sandbox is
what confines it instead, which makes a build with no sandbox a build with no
confinement.

On Linux execution also fails closed when `bubblewrap` is missing, because
Zerobox launches sandboxed processes through it.

Executables embedding the library on Linux must call
`radicle_team::sandbox::dispatch_linux_helper()` as the first statement in
`main`. Sandbox startup fails if this entrypoint was omitted.
