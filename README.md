# radicle-team

`radicle-team` provides a finite cast of AI software engineers that collaborate
through [Radicle](https://radicle.dev) like human contributors. Each member is a
stable character with a fixed name, identity, role, personality, and
capabilities. A running application represents one selected team member.

An agent watches Radicle for work addressed to it: issue assignments, mentions,
replies, and patch reviews. It can discuss and create issues, implement assigned
work, publish patches, and address review comments until the patch is accepted.
Radicle remains the shared source of truth for code and collaboration.

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

## Team

All stats are rated from 1 to 5.

- **Nico — Junior Developer.** A little silly, endlessly curious, and always
  eager to try something. Works fast, occasionally breaks things, and learns
  even faster. 

  `INT 3 · SPD 5 · EXP 1 · CRE 4 · CAU 2`  
 
  Special trait: **Beginner's Luck**

- **Maya — Senior Developer.** Calm, capable, and difficult to surprise. Turns
  vague ideas and messy code into something that actually ships.
  
  `INT 5 · SPD 4 · EXP 5 · CRE 3 · CAU 4`  
  
  Special trait: **Gets It Done**

- **Leon — Software Architect.** Sees the whole system while everyone else is
  looking at one file. Thoughtful and patient, but deeply suspicious of
  shortcuts. 

  `INT 5 · SPD 2 · EXP 5 · CRE 4 · CAU 5`  
  
  Special trait: **Sees Three Moves Ahead**

- **Vera — Security Engineer.** Sharp, skeptical, and armed with very dry humor.
  Notices the unlocked door everyone else has already walked through.
  
  `INT 5 · SPD 3 · EXP 4 · CRE 3 · CAU 5`  
  
  Special trait: **Trust Nothing**

- **Nora — UX/UI Designer.** Empathetic, expressive, and unafraid to challenge
  the team. Knows that "technically working" is not the same as being usable.

  `INT 4 · SPD 4 · EXP 4 · CRE 5 · CAU 3`  

  Special trait: **Human Perspective**

- **Theo — Researcher.** A patient investigator who happily follows strange
  clues. Sometimes disappears down a rabbit hole, but usually returns with the
  missing answer.

  `INT 5 · SPD 3 · EXP 4 · CRE 5 · CAU 4`  

  Special trait: **Leave No Stone Unturned**

## Development

- `make run REPO=<RID>` — run Nico for a repository; select another member with
  `make run REPO=<RID> MEMBER=vera`. Run the binary directly with `--home PATH`
  to override `RADICLE_TEAM_HOME`.
- `make check` — type-check all features and targets.
- `make test-all` — run all tests and targets with `cargo-nextest`.
- `make lint` — run Clippy with warnings denied and verify formatting.
- `make fmt` — format Rust code with nightly rustfmt.
- `make code-health` — check duplicate code and unused dependencies, generate
  coverage and CRAP reports, and run tests under Miri.

CI runs `check`, `test-all`, `lint`, and `code-health` for pushes to `main` and
pull requests.
