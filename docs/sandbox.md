# Local sandbox

Agents run inside bubblewrap on Linux or Seatbelt on macOS. They bypass
interactive permission prompts, so operating-system confinement is required.
Unsupported platforms refuse to compile. Linux also requires `bwrap` at runtime.

Setup creates `<member home>/sandbox.toml`. It is administrator-owned and
reloaded before every sandboxed command. Invalid or unsafe edits fail the next
spawn. Running commands keep their original policy.




## Filesystem paths

```toml
version = 1
allow_read = ["/custom/toolchain"]
deny_read = ["/private/project"]
allow_write = ["/custom/build-cache"]
deny_write = ["/custom/build-cache/protected"]
network = "all"
```

Paths must be absolute. Local entries extend or restrict the embedded Zerobox
policy, with denials taking precedence. Common Rust, Node.js, Python, Go, and
Nix toolchain paths are enabled.

An agent can write its role's worktree and harness, its member's shared provider
state and cache, and the necessary Git object store. It cannot change standing
instructions, signing identities, Git configuration, hooks, worktree pointers,
or other roles' references. Coder's branches are confined to
`refs/heads/<member>-rt/`.

Caches and provider state are shared between one member's roles. Do not share
build caches between members: cached build scripts and plugins execute as the
next user of that cache.

Runtime Git uses a separate sandbox with no external network access or access to
signing keys. It can write the target worktree and required Git state, and read
the source repository in Radicle storage. Operator additions to `sandbox.toml`
do not widen this runtime-Git policy.




## Network

Use `"all"` (the default), `"disabled"`, or a list of allowed domains:

```toml
network = ["api.example.com", "crates.io"]
```

A domain list must cover the provider API and the
[adapter's dependencies](adapters/index.md). The member's capability-protected
MCP endpoint remains reachable in every mode.




## Loopback

On **macOS**, host loopback remains reachable even with disabled or restricted
networking. Local services need their own authentication if agents must not
access them. This also applies to runtime Git.

On **Linux**, disabled and domain-restricted modes use a network namespace per
command. Host loopback and new Unix domain sockets are unavailable. A
development server started by one command cannot be reached by another. Such
workflows need `network = "all"`.

For restricted internet access with working local services on Linux, run the
member in a container with `network = "all"` and filter container egress. Keep
Radicle seeds reachable for the managed node.

Executables embedding the library on Linux must call
`radicle_team::sandbox::dispatch_linux_helper()` first in `main`.
