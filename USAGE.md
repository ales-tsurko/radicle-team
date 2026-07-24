# Usage guide

`radicle-team` runs one selected team member as an isolated Radicle identity and
node. The same machine can run multiple members as separate processes.

## Members

All provider-neutral tuning values are rated from 1 to 5. `INT` represents
preferred model capability, `SPD` response speed, `EXP` engineering autonomy,
`CRE` solution exploration, and `CAU` risk-aware deliberation. Agent adapters
receive these values at startup and map them to provider-specific controls.

- **Nico — Junior Developer.** A little silly, endlessly curious, and eager to
  try something. Works fast, occasionally breaks things, and learns even
  faster. 

  `INT 3 · SPD 5 · EXP 1 · CRE 4 · CAU 2`

  Special trait: **Beginner's Luck**.

- **Maya — Senior Developer.** Calm, capable, and difficult to surprise. Turns
  vague ideas and messy code into something that ships.

  `INT 5 · SPD 3 · EXP 5 · CRE 4 · CAU 4`

  Special trait: **Gets It Done**.

- **Leon — Software Architect.** Sees the whole system while everyone else is
  looking at one file. Thoughtful, patient, and suspicious of shortcuts.

  `INT 5 · SPD 2 · EXP 5 · CRE 4 · CAU 5`

  Special trait: **Sees Three Moves Ahead**.

- **Vera — Security Engineer.** Sharp, skeptical, and armed with dry humor.
  Notices the unlocked door everyone else has walked through.

  `INT 5 · SPD 2 · EXP 5 · CRE 3 · CAU 5`
  
  Special trait: **Trust Nothing**.

- **Nora — UX/UI Designer & Frontend Developer.** Empathetic, expressive, and
  unafraid to challenge the team. Knows that technically working is not the same
  as usable. 

  `INT 4 · SPD 4 · EXP 4 · CRE 5 · CAU 3`

  Special trait: **Human Perspective**.

- **Theo — Researcher.** A patient investigator who follows strange clues.
  Sometimes disappears down a rabbit hole, but usually returns with the missing
  answer. 

  `INT 5 · SPD 1 · EXP 5 · CRE 4 · CAU 5`

  Special trait: **Leave No Stone Unturned**.

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
member's Radicle home.

If the repository manifest or member binding is missing, run setup from the
repository checkout. It shows the exact Git commands and asks before writing
`.radicle-team.toml`, creating a commit, and opening a Radicle patch with the
user's identity. After that patch reaches the canonical branch, the wizard
validates it and finishes with:

```text
nico is ready for rad:... with binding generation 1
```

Setup publishes only when `HEAD` exactly matches the live canonical `rad` tip.
Switch to an up-to-date canonical branch first so unrelated commits and patches
are never included.

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

The process owns the member's node for its lifetime. It classifies relevant
assignments, mentions, replies, comments, reviews, revisions, reactions,
resolutions, lifecycle changes, and merges until stopped. `SIGINT` and
`SIGTERM` trigger graceful node shutdown before the child is reaped.

## Repository manifest

Exactly one `.radicle-team.toml` file is active. The default is the file on the
repository's canonical branch. Use `--manifest PATH` to select an explicit file
with the same schema instead; the two sources are never merged.

During interactive setup, the wizard asks before updating an explicit manifest
at `PATH`, creating it when absent. It updates that file directly and does not
create a repository patch. New explicit manifests use mode `0600` on Unix.

The repository manifest defines shared policy: every contributor sees the same
reviewed mapping between members and DIDs. Machine-private identity material,
logs, locks, and watcher state remain under `~/.radicle-team`.

```toml
version = 1

[members.nico]
signing_did = "did:key:z6Mk..."
generation = 1
state = "active"
mcp_allow = ["read", "comments", "patches", "reactions", "reviews"]
```

`mcp_allow` accepts exact MCP method names and the fixed `read`, `comments`,
`issues`, `patches`, `reactions`, and `reviews` groups. The `patches` group does
not include `merge_patch`; allow that method explicitly. `mcp_allow` is required,
while an empty list disables every tool.

Groups are derived from the tools registered by the MCP server: `get_*` and
`list_*` tools are reads; non-read tools are grouped by the `comment`, `issue`,
`patch`, `reaction`, or `review` component in their name. MCP `tools/list` is the
authoritative method list.

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

Managed Radicle commands ignore an inherited `RAD_SOCKET`; each member always
uses the control socket inside its isolated Radicle home.

If setup fails for a private repository, use the DID shown in the error or
identity log message to grant access, then rerun setup. Node startup failures
include the log path in their diagnostic.
