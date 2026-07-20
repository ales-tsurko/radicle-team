# radicle-team

`radicle-team` provides a finite cast of AI software engineers that collaborate
through [Radicle](https://radicle.dev) like human contributors. Each member is a
stable character with a fixed name, identity, role, personality, and
capabilities. A running application represents one selected team member.

A running member watches Radicle for work addressed to it: issue assignments,
mentions, replies, and patch reviews. The current runtime classifies and reports
that activity without modifying collaborative objects. Radicle remains the
shared source of truth for code and collaboration.

The project provides a reusable library and a thin binary. Each selected member
runs as a separate process with its own Radicle node and identity. The whole
team can run on one local machine or be distributed across servers; members
coordinate only through Radicle.

Member data is isolated under
`$RADICLE_TEAM_HOME/members/<member>/`; `RADICLE_TEAM_HOME` defaults to
`~/.radicle-team`. Each member directory contains its Radicle home and watcher
state database, allowing all members to run on the same machine.

The watcher recognizes every issue and patch interaction relevant to the
member, including assignments, mentions, replies, comments, reviews,
revisions, reactions, resolutions, lifecycle changes, and merges.

## Quick start

Install Radicle with `rad` and `radicle-node` on `PATH`, then prepare a member:

```sh
radicle-team setup nico --repo rad:...
```

The setup wizard provisions the identity, handles private-repository access, and
offers to prepare the required manifest change. Repository manifests are
published as patches. Once setup reports that Nico is ready, start the member:

```sh
radicle-team run nico --repo rad:...
```

See the [usage guide](USAGE.md) for private repositories, multiple members,
identity options, and troubleshooting.

## Repository manifest

Each project uses one `.radicle-team.toml` manifest. By default it is loaded
from the repository's canonical branch. `--manifest PATH` selects an explicit
file with the same schema; sources are never combined.

```toml
version = 1

[members.nico]
signing_did = "did:key:z6Mk..."
generation = 1
state = "active"
```

The file contains project settings, including member identity bindings. Private
keys and passphrases are never stored in it.

## Team

All stats are rated from 1 to 5. `INT` represents model capability, `SPD`
response speed, `EXP` engineering autonomy, `CRE` solution exploration, and
`CAU` risk-aware deliberation.

- **Nico — Junior Developer.** A little silly, endlessly curious, and always
  eager to try something. Works fast, occasionally breaks things, and learns
  even faster.

  `INT 3 · SPD 5 · EXP 1 · CRE 4 · CAU 2`

  Special trait: **Beginner's Luck**

- **Maya — Senior Developer.** Calm, capable, and difficult to surprise. Turns
  vague ideas and messy code into something that actually ships.

  `INT 5 · SPD 3 · EXP 5 · CRE 4 · CAU 4`

  Special trait: **Gets It Done**

- **Leon — Software Architect.** Sees the whole system while everyone else is
  looking at one file. Thoughtful and patient, but deeply suspicious of
  shortcuts.

  `INT 5 · SPD 2 · EXP 5 · CRE 4 · CAU 5`

  Special trait: **Sees Three Moves Ahead**

- **Vera — Security Engineer.** Sharp, skeptical, and armed with very dry humor.
  Notices the unlocked door everyone else has already walked through.

  `INT 5 · SPD 2 · EXP 5 · CRE 3 · CAU 5`

  Special trait: **Trust Nothing**

- **Nora — UX/UI Designer & Frontend Developer.** Empathetic, expressive, and
  unafraid to challenge the team. Knows that "technically working" is not the
  same as being usable.

  `INT 4 · SPD 4 · EXP 4 · CRE 5 · CAU 3`

  Special trait: **Human Perspective**

- **Theo — Researcher.** A patient investigator who happily follows strange
  clues. Sometimes disappears down a rabbit hole, but usually returns with the
  missing answer.

  `INT 5 · SPD 1 · EXP 5 · CRE 4 · CAU 5`

  Special trait: **Leave No Stone Unturned**




## Development

Contributions follow the repository [style guide](STYLE.md).

- `make check` — type-check all features and targets.
- `make test-all` — run all tests and targets with `cargo-nextest`.
- `make lint` — run Clippy with warnings denied and verify formatting.
- `make fmt` — format Rust code with nightly rustfmt.
- `make code-health` — check duplicate code and unused dependencies, generate
  coverage and CRAP reports, and run tests under Miri.

CI runs `check`, `test-all`, `lint`, and `code-health` for pushes to `main` and
pull requests.
