# Usage guide

`radicle-team` runs one selected team member as an isolated Radicle identity
and node. The same machine can run multiple members as separate processes.




## Prerequisites

Install Radicle so that `rad` and `radicle-node` are available on `PATH`. You
also need the repository RID and permission to propose or approve its project
manifest.

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
repository, and prints the DID. For private repositories, the wizard waits
while an authority grants that DID access and then retries synchronization.
Setup also records a persistent seeding policy for the repository in the
member's Radicle home, so later watcher synchronization remains enabled.

If the repository manifest or member binding is missing, run setup from the
repository checkout. It shows the exact Git commands and asks before writing
`.radicle-team.toml`, creating a commit, and opening a Radicle patch with the
user's identity. After that patch reaches the canonical branch, the wizard
validates it and finishes with a message such as:

```text
nico is ready for rad:... with binding generation 1
```

Setup publishes only when `HEAD` exactly matches the live canonical `rad` tip.
Switch to an up-to-date canonical branch first, so unrelated commits and
patches are never included.

For a private repository, its authority must grant the printed DID access
before synchronization can succeed. A known seed can be supplied explicitly:

```sh
radicle-team setup nico --repo rad:... --seed z6Mk...
```

`run` synchronizes before validating the manifest and starting the watcher.
Passing `--seed` uses those peers for that synchronization.

When standard input or output is not a terminal, setup does not prompt or
modify the checkout. It prints the required action and exits instead.




## Run a member

After setup reports that the member is ready, start the watcher:

```sh
radicle-team run nico --repo rad:...
```

The process owns the member's node for its lifetime. It watches relevant issue
and patch activity until stopped. `SIGINT` and `SIGTERM` are forwarded as a
graceful node shutdown before the child is reaped.




## Repository manifest

Exactly one `.radicle-team.toml` file is active. The default is the file on the
repository's canonical branch. Use `--manifest PATH` to select an explicit file
with the same schema instead; the two sources are never merged.

During interactive setup, the wizard asks before updating an explicit manifest
at `PATH`, creating it when absent. It updates that file directly and does not
create a repository patch. New explicit manifests use mode `0600` on Unix.

The file belongs to the repository because it defines shared policy: every
contributor must see the same reviewed mapping between characters and DIDs.
`~/.radicle-team` is reserved for machine-private identity material, logs,
locks, and local watcher state.

```toml
version = 1

[members.nico]
signing_did = "did:key:z6Mk..."
generation = 1
state = "active"
```

Invocation-specific options remain command-line arguments:

- `--home PATH` overrides `RADICLE_TEAM_HOME`;
- `--seed NID` adds a preferred synchronization seed and may be repeated;
- `--identity-home PATH` uses an existing dedicated Radicle identity;
- `--expected-did DID` verifies that existing identity; and
- `--unencrypted-identity` creates a managed key protected only by filesystem
  permissions.

An encrypted `--identity-home` must provide its passphrase through
`RAD_PASSPHRASE` or the matching operating-system credential. SSH-agent access
alone cannot unlock the managed `radicle-node` process.




## Run multiple members

Set up and run each character independently:

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

Managed Radicle commands ignore an inherited `RAD_SOCKET`; each member always
uses the control socket inside its isolated Radicle home.

If setup fails for a private repository, use the DID shown in the error or
identity log message to grant access, then rerun setup. Node startup failures
include the log path in their diagnostic.
