# Local sandbox

Setup creates an administrator-owned sandbox configuration for each member:

```text
~/.radicle-team/repositories/<canonical-RID>/members/<member>/sandbox.toml
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




## Filesystem paths

Radicle Team loads Zerobox's embedded platform and sensitive-path policies
directly, so user-defined Zerobox profiles cannot replace them. It also enables
common Rust, Node.js, Python, Go, and Nix toolchain paths and grants the
member's ephemeral workspaces. Local path entries extend or restrict those
defaults, and denied paths take precedence. Paths in the file must be
absolute.

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
that is still a restricted sandbox. A restricted sandbox on Linux also
blocks the command from creating new Unix domain sockets, so local services
reached that way, such as a container daemon or a database socket, are
unavailable there as well.

If you need both an isolated internet and a working local server on Linux, run
the whole member inside a container with `network = "all"` and filter egress at
the container instead. Leave the container able to reach your Radicle seeds, or
`radicle-node` cannot synchronize.




## Platform support

Agent execution currently fails closed outside macOS and Linux because Zerobox
does not provide an enforcing backend there. On Linux it also fails closed when
`bubblewrap` is missing, because Zerobox launches sandboxed processes through
it.

Executables embedding the library on Linux must call
`radicle_team::sandbox::dispatch_linux_helper()` as the first statement in
`main`. Sandbox startup fails if this entrypoint was omitted.
