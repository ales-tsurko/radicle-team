# Setup and operation

Installing `radicle-team`, provisioning a member, running it, and finding things
when something goes wrong. Configuration lives in
[the manifest reference](manifest.md) and [the sandbox reference](sandbox.md).




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

- activity dispatched to an adapter, or ignored and for which reason
- a mutation published, or recovered from an interrupted delivery
- an MCP method the manifest no longer allows
- the sandbox becoming ready

`debug` adds each repository scan, each classified activity, and the policy
built for every sandboxed command.

The ignored-activity records matter most in practice. Without them a
`triggered_by` decision, or a conversation that has run its turns, looks exactly
like a member that is simply not responding.




## Run multiple members

Set up and run each member independently:

```sh
radicle-team run nico --repo rad:...
radicle-team run maya --repo rad:...
```

Their identities, nodes, storage, and state are isolated below:

```text
~/.radicle-team/repositories/<canonical-RID>/members/<member>/
```

Two processes cannot use the same repository-member home concurrently. The
same named member can run for different repositories without sharing identity
or state.




## Command-line options

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




## Local state

Managed private keys live in the member's Radicle home. Encrypted-key
passphrases live in the operating-system credential store, never in the
manifest. Node output is written to:

```text
~/.radicle-team/repositories/<canonical-RID>/members/<member>/node.log
```

Managed Radicle commands ignore an inherited `RAD_SOCKET`. Each member always
uses the control socket inside its isolated Radicle home.




## Troubleshooting

If setup fails because a repository is private, use the DID shown in the error
or identity log message to grant access, then rerun setup. Node startup failures
include the log path in their diagnostic.
