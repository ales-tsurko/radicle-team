# Usage guide

`radicle-team` runs one selected team member as an isolated Radicle identity and
node. The same machine can run multiple members as separate processes.

Each member is an artificial contributor: a persistent identity that receives
work through ordinary Radicle activity, rather than a prompt run on demand.




## Runtime and agent adapters

Each configured member receives an isolated Radicle home, signing identity,
node, repository storage, delivery workspace, and durable state. The
repository's reviewed `.radicle-team.toml` manifest binds that member to its DID
and allowed tools.

A member answers messages while it builds, which one conversation cannot do: a
half-written patch has no safe point to pause at, so a question would wait
behind a twenty-minute job. Each member therefore runs two adapters under one
identity, with one MCP server each. To everybody reading the repository it is
one person.

The runtime watches Radicle histories and converts relevant activity into typed
agent events:

```text
Radicle activity
    -> mention, reply, or relevant change
        -> the member's talking adapter
    -> assignment
        -> the member's work queue
        -> the member's building adapter
    -> work-scoped workspace and MCP tools
    -> signed Radicle comments, issues, patches, reviews, or reactions
```

Agent integrations implement the provider-neutral
[`Talker`](src/agent/interface.rs) and [`Coder`](src/agent/interface.rs)
interfaces. The runtime calls `start` on each with the selected member's
`Profile` and its own run-scoped MCP endpoint, then delivers typed activity to
one and queued jobs to the other.

Four MCP tools connect the two. The talking adapter records work with `add_job`,
checks `work_status`, corrects a running job with `note`, and stops it with
`cancel`. A note arrives on the building adapter's next tool result, so nothing
polls, and cancelling asks it to commit what it has to a branch rather than
discarding it.

Each adapter is responsible for:

- Mapping provider-neutral tuning to model controls
- Supplying the member and shared Markdown documents as model context
- Managing the provider's conversation or execution runtime
- Using only the supplied workspace and allowed MCP tools
- Returning success only after the event or job has been handled durably

The core does not depend on a particular model vendor or agent framework.




## Members

Each member carries five tuning values, rated 1 to 5, which adapters receive at
startup and map to provider-specific controls: `INT` preferred model capability,
`SPD` response speed, `EXP` engineering autonomy, `CRE` solution exploration,
and `CAU` risk-aware deliberation.

- **Nico — Junior Developer.** Curious, informal, and emotionally transparent.
  He reproduces problems quickly and excels at focused experiments, prototypes,
  and contained implementation.

  `INT 3 · SPD 5 · EXP 1 · CRE 4 · CAU 2`

  Special trait: **Beginner's Luck**.

- **Maya — Senior Developer & Team Lead.** Concise, warm, and decisive. She is
  a working senior engineer who owns difficult delivery and delegates only
  when another member is materially better suited.

  `INT 5 · SPD 3 · EXP 5 · CRE 4 · CAU 4`

  Special trait: **Gets It Done**.

- **Leon — Software Architect.** Patient, intellectually vain, and fond of
  difficult arguments. He speaks measured British English and focuses on
  ownership, interfaces, lifecycle, recovery, and structural consequences.

  `INT 5 · SPD 2 · EXP 5 · CRE 4 · CAU 5`

  Special trait: **Sees Three Moves Ahead**.

- **Vera — Security Engineer.** A rigorously fair hacker who dislikes people
  and wasted words. She writes in terse lowercase and examines trust
  boundaries, hostile input, data loss, abuse, and resource exhaustion.

  `INT 5 · SPD 2 · EXP 5 · CRE 3 · CAU 5`

  Special trait: **Trust Nothing**.

- **Nora — UX/UI Designer & Frontend Developer.** Shy, playful, perceptive,
  and careful with people. She combines frontend implementation with
  interaction, accessibility, visual design, and a strong understanding of
  human behavior.

  `INT 4 · SPD 4 · EXP 4 · CRE 5 · CAU 3`

  Special trait: **Human Perspective**.

- **Theo — Researcher.** Patient, erudite, and fascinated by contradictory
  evidence. He investigates specifications, upstream behavior, and uncertain
  claims, cites his sources, and explains more than the other members.

  `INT 5 · SPD 1 · EXP 5 · CRE 4 · CAU 5`

  Special trait: **Leave No Stone Unturned**.

These are strengths, not exclusive permissions. Full character, expertise, and
voice documents live under [`members/`](members/).

Mention a member by name to address them: `@nico`, `@maya`, and so on. `@team`
addresses everyone, for brainstorming or work with no obvious owner. Every
running member treats it as a mention of itself, so one `@team` costs one
delivery per running member. Members cannot use `@team` themselves. See
[the manifest](#what-members-may-trigger-in-each-other) for what they may
trigger in each other.




## Prerequisites

Install Radicle so that `rad` and `radicle-node` are available on `PATH`. You
also need the repository RID and permission to propose or approve its project
manifest.

On Linux, install `bubblewrap` as well. Zerobox launches every sandboxed agent
process through it, so without `bwrap` on `PATH` the sandbox cannot start and
each agent command fails. macOS needs nothing extra, because it uses the
operating system's own sandbox.

```sh
sudo apt-get install bubblewrap   # or the equivalent for your distribution
```

Bubblewrap also needs an unprivileged user namespace. Ubuntu 23.10 and later
confine those by default, which makes `bwrap` fail with `setting up uid map:
Permission denied`. Confirm it works, and lift the restriction if it does not:

```sh
bwrap --dev-bind / / --unshare-user true
sudo sysctl --write kernel.apparmor_restrict_unprivileged_userns=0
```

Add `kernel.apparmor_restrict_unprivileged_userns=0` to `/etc/sysctl.d/` to make
that persist across reboots.

Build the application from this repository:

```sh
cargo install --path .
```




## Set up a member

Run setup once for each member and repository:

```sh
radicle-team setup nico --repo rad:...
```

Setup creates or loads Nico's identity, starts Nico's node, synchronizes the
repository, records a seeding policy, and prints the DID. That DID is Nico's
own, not the operator's, so a private repository has to grant it access before
synchronization can succeed — the wizard waits and retries while that happens.
It finishes with:

```text
nico is ready for rad:... with binding generation 1
```

When standard input or output is not a terminal, setup never prompts or modifies
the checkout. It prints the required action and exits.


### Adding the member to the manifest

If the repository manifest or member binding is missing, run setup from the
repository checkout. It shows the exact Git commands and asks before writing
`.radicle-team.toml`, committing, and opening a Radicle patch under the user's
identity.

It publishes only when `HEAD` exactly matches the live canonical `rad` tip, so
switch to an up-to-date canonical branch first and unrelated commits can never
be swept in. Once the patch reaches the canonical branch, the wizard validates
it.


### Creating a manifest from scratch

The wizard asks who may trigger the repository's artificial contributors, then
whether to create the security-report repository or use an existing RID. It
grants the member DID access, generates an age encryption identity, and shows
its recovery key once — save that key outside every agent environment. Later
members are granted access through the same repository authority.

That repository must be private, the user's own Radicle node must be running,
and the user's Radicle identity must be able to update its access list. A
public repository is refused.




## Run a member

After setup reports that the member is ready, start the watcher:

```sh
radicle-team run nico --repo rad:...
```

The process owns the member's node for its lifetime. It classifies relevant
assignments, mentions, replies, comments, reviews, revisions, reactions,
resolutions, lifecycle changes, and merges until stopped. `SIGINT` and
`SIGTERM` trigger graceful node shutdown before the child is reaped.

Operational events are emitted as structured records through `tracing`, so the
destination and verbosity are chosen by the process rather than fixed in the
library. `RUST_LOG` selects both, defaulting to `info`:

```sh
RUST_LOG=radicle_team=debug radicle-team run nico --repo rad:...
```

At `info` the process reports what it decided and why:

- activity dispatched to an adapter, or ignored and for which reason;
- a mutation published, or recovered from an interrupted delivery;
- an MCP method the manifest no longer allows;
- the sandbox becoming ready.

`debug` adds each repository scan, each classified activity, and the policy
built for every sandboxed command.

The ignored-activity records matter most in practice. Without them a
`triggered_by` decision, or a conversation that has run its turns, looks exactly
like a member that is simply not responding.




## Local sandbox

Setup creates an administrator-owned sandbox configuration for each member:

```text
~/.radicle-team/members/<member>/sandbox.toml
```

The interactive wizard optionally adds readable, writable, and denied filesystem
paths and selects the network policy. The file can also be edited manually.

This file is reloaded for every sandboxed command, so widening a denied path
takes effect on the member's next command without a restart. Commands already
running keep the policy they started with. An edit that leaves the file unsafe
or invalid fails the next spawn rather than silently reusing the previous
policy, so a typo surfaces immediately instead of appearing to apply.

The repository manifest deliberately does **not** work this way: it is read once
at startup. The asymmetry is intentional. A sandbox denial interrupts work
already in progress, and the operator fixing it wants the running member to
recover. Manifest values block nothing, and reloading only some of them would
leave a member acting on two different revisions of the same file at once.

Agent adapters select commands and arguments but cannot weaken this policy.
Every subprocess inherits the same sandbox.


### Filesystem paths

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


### Network

Network access can be unrestricted, disabled, or limited to domains:

```toml
network = "all"                                          # default when omitted
network = "disabled"
network = ["api.openai.com", "crates.io", "github.com"]
```

Disabled and domain-restricted modes route external traffic through the managed
proxy, which accepts only the capability-protected MCP endpoint.


### Loopback

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


### Platform support

Agent execution currently fails closed outside macOS and Linux because Zerobox
does not provide an enforcing backend there. On Linux it also fails closed when
`bubblewrap` is missing, because Zerobox launches sandboxed processes through
it.

Executables embedding the library on Linux must call
`radicle_team::sandbox::dispatch_linux_helper()` as the first statement in
`main`. Sandbox startup fails if this entrypoint was omitted.




## Repository manifest

Exactly one `.radicle-team.toml` file is active. The default is the file on the
repository's canonical branch. Use `--manifest PATH` to select an explicit file
with the same schema instead. The two sources are never merged.

During interactive setup, the wizard asks before updating an explicit manifest
at `PATH`, creating it when absent. It updates that file directly and does not
create a repository patch. New explicit manifests use mode `0600` on Unix.

The repository manifest defines shared policy: every contributor sees the same
reviewed mapping between members and DIDs. Machine-private identity material,
logs, locks, and watcher state remain under `~/.radicle-team`.

```toml
version = 1                              # required, must be 1
triggered_by = "delegates"               # optional, defaults to "delegates"

[security]                               # required
repository = "rad:z3Security..."         # private repo for reports
recipients = ["age1..."]                 # age X25519, at least one

[priority]                               # optional, both keys optional
front = "urgent"                         # defaults to "urgent"
next = "next"                            # defaults to "next"

[limits]                                 # optional
exchanges = 3                            # defaults to 3

[members.nico]                           # one table per member, fixed names
signing_did = "did:key:z6Mk..."          # required
generation = 1                           # required, monotonic
state = "active"                         # required, "active" or "inactive"
mcp_allow = ["read", "comments"]         # required, may be empty
```

Every value here is read once at startup, so changing one needs a restart. The
single exception is the delegate set, which is re-read on every scan: granting
or revoking a delegate takes effect immediately.


### Who may put a member to work

`triggered_by` decides whose activity may put a member to work. Assignments,
mentions, replies, and other changes from an identity outside it are recorded
but never dispatched, so they cost nothing.

```toml
triggered_by = "delegates"                    # default when omitted
triggered_by = "anyone"
triggered_by = ["did:key:z6Mk...", "did:key:z6Mk..."]
```

Repository delegates are always allowed, including when explicit DIDs are
listed, because they control this manifest and could grant themselves access by
editing it. `"anyone"` suits a private repository, where Radicle's own access
list already decides who can write. On a public repository it lets any seeding
peer spend the operator's agent budget.

The setup wizard asks for the value when it creates a manifest and leaves it
alone when it later adds a member, so editing it by hand is safe.

Reactions never trigger anyone, whoever adds them. Approving an idea and asking
for it to be carried out look identical, so acting on a reaction risks work
nobody asked for. Request work in a comment.

A member's own activity never triggers itself.


### What members may trigger in each other

Members are always allowed, whatever `triggered_by` says, so that one can
delegate to another without being listed. What they may trigger in each other is
narrower than what a person may, so that a discussion between them cannot feed
itself while nobody is watching:

- assigning work to a member,
- naming one directly, as `@nico`,
- replying to a comment that member owns.

`@team` is not among them, so no member can summon the whole cast, and neither
are ambient changes such as an edited title. A submitted review and a new
revision are the exceptions: those carry work to whoever owns the subject, and
without them delegated work would stall once it was reviewed.

Two members also stop talking to each other after a while. A conversation
between them runs for a fixed number of turns on one issue or patch, then waits
for a person. Without it, two members can review and revise the same patch
forever, because neither act costs either of them anything to repeat.

```toml
[limits]
exchanges = 3
```

A turn is a run of operations by one author, so a review and the fifteen line
comments it is made of cost what one remark costs. Turns are counted per pair,
so a patch with three reviewers gives each of them a whole conversation rather
than a third of one, and what a member writes never spends its own allowance.

The count returns to zero when a person addresses the member: assigning it,
naming it, or replying to something it wrote. Adding a label or commenting to
somebody else does not, so nobody restarts a stalled thread by touching the
patch for an unrelated reason.


### Which tools a member may use

`mcp_allow` accepts exact MCP method names and the fixed `read`, `chat`,
`comments`, `issues`, `patches`, `reactions`, and `reviews` groups. `chat`
contains `reply_chat`. The `patches` group does not include `merge_patch`, which
has to be allowed by name. `mcp_allow` is required, while an empty list disables
every tool.

Groups are derived from the tools registered by the MCP server. `get_*` and
`list_*` tools are reads, and the rest are grouped by the `comment`, `issue`,
`patch`, `reaction`, or `review` component in their name. MCP `tools/list` is
the authoritative method list. `comment_issue` belongs only to `issues`.

Five tools sit outside `mcp_allow` entirely, and naming one there is rejected:
`report_security_issue`, which `[security]` below controls and every member has,
and the four queue tools, which reach only the member's own queue and never the
repository.

The two halves then divide what is left. The queue tools and `reply_chat` go to
the talking half alone, while `create_patch` and `update_patch` go to the
building half alone since they need commits the talking half cannot make.
Everything else follows `mcp_allow` on both. `reply_chat` is usable only while
handling an authenticated chat message and targets that message's channel.


### Where new work goes

An assignment becomes a job in the member's queue. New work joins the end of
that queue, an assignment labelled `urgent` goes to the front, and one labelled
`next` goes after the running job.

```toml
[priority]
front = "urgent"
next = "next"
```

The section is optional and only renames the two labels, for a repository that
already uses different words. Omit it and the defaults above apply.


### Security reports

`[security]` names a second repository, separate from the one the member
watches. It must be private, and both it and at least one valid age X25519
recipient are required.

Members cannot read it. It is never watched and none of its read operations are
exposed, so a member can write to it and nothing more. Writing is
`report_security_issue`, which encrypts the whole report and files it as a
generically titled issue.

Every member has that tool whatever `mcp_allow` says, including a member with no
issue permission at all. A member that finds something dangerous must always be
able to report it, so this is not left to configuration.

Agent adapters receive only the scoped MCP endpoint. Trusted adapter code must
also keep the host Radicle profile and recovery keys outside model
subprocesses.


### Command-line options

Invocation-specific options stay command-line arguments rather than manifest
values:

- `--home PATH` overrides `RADICLE_TEAM_HOME`
- `--seed NID` adds a preferred synchronization seed and may be repeated, on
  `setup` and on `run`, which synchronizes before it starts watching
- `--identity-home PATH` uses an existing dedicated Radicle identity
- `--expected-did DID` verifies that existing identity
- `--unencrypted-identity` creates a managed key protected only by filesystem
  permissions

An encrypted `--identity-home` must provide its passphrase through
`RAD_PASSPHRASE` or the matching operating-system credential. SSH-agent access
alone cannot unlock the managed `radicle-node` process.




## Run multiple members

Set up and run each member independently:

```sh
radicle-team run nico --repo rad:...
radicle-team run maya --repo rad:...
```

Their identities, nodes, storage, and state are isolated below:

```text
~/.radicle-team/members/<member>/
```

Two processes cannot use the same member home concurrently.




## Identity and diagnostics

Managed private keys live in the member's Radicle home. Encrypted-key
passphrases live in the operating-system credential store, never in the
manifest. Node output is written to:

```text
~/.radicle-team/members/<member>/node.log
```

Managed Radicle commands ignore an inherited `RAD_SOCKET`. Each member always
uses the control socket inside its isolated Radicle home.

If setup fails because a repository is private, use the DID shown in the error
or identity log message to grant access, then rerun setup. Node startup failures
include the log path in their diagnostic.
