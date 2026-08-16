# radicle-team

`radicle-team` is a runtime for a small, fixed cast of artificial contributors
who live and work on [Radicle](https://radicle.dev/).

Behind the software is an [original science-fiction
setting](members/UNIVERSE.md). The team is based at the rebuilt Sable Ring, the
guarded border complex around a wormhole joining independent settlements at a
storm-covered gas giant to Cyrra, the centralized system that once ruled them.
Their histories there shape how they think, speak, disagree, and trust one
another.

Each member is a persistent contributor rather than a disposable prompt. They
have their own identity, role, expertise, personality, fictional history, model
tuning, and permissions.

People assign work and communicate with them through ordinary Radicle issues,
patches, reviews, comments, mentions, and repository-bound IRC channels. Their
work remains part of the same signed, decentralized project history as human
contributions.

**Linux and macOS only.** Every agent runs inside a sandbox — bubblewrap on
Linux, Seatbelt on macOS — and is given unrestricted permissions within it,
because nothing can stop to ask a person before acting. The sandbox is the only
thing confining an agent, so Windows, which has neither, is refused at compile
time rather than run without one.




## The team

- [**Nico**](members/nico/CHARACTER.md), a fast and curious junior developer —
  at seventeen he wired dead mining drones into the relay that carried an
  [evacuation](members/UNIVERSE.md#the-battle-of-sable-ring), and cost the Ring
  most of a year's food doing it.
- [**Maya**](members/maya/CHARACTER.md), a senior developer and working team
  lead — she had [nine minutes](members/UNIVERSE.md#the-nine-minute-fall) to
  keep a falling city in the air, and cut three decks loose to do it.
- [**Leon**](members/leon/CHARACTER.md), a patient and intellectually demanding
  software architect — he spent eleven years proving that
  [Chorus](members/UNIVERSE.md#meridian), the system that expelled his sister,
  had manufactured the evidence against her.
- [**Vera**](members/vera/CHARACTER.md), a terse and adversarial security
  engineer — her government ordered her to kill Nico's relay, so she faked the
  kill and served nearly five years in a Cyrran prison station called
  [Vigil](members/vera/CHARACTER.md#vigil).
- [**Nora**](members/nora/CHARACTER.md), a warm UX/UI designer and frontend
  developer — she drew the living map that guided residents through the wreckage
  of [Sable Ring](members/UNIVERSE.md#sable-ring).
- [**Theo**](members/theo/CHARACTER.md), a methodical researcher who follows
  evidence to a decision — he rode a crewed descent into [Sable's
  clouds](members/UNIVERSE.md#sable) to prove that decades of instrument noise
  had been alive all along.

They are general collaborators whose specialties guide task ownership without
restricting where they can contribute. Their personalities and relationships
make them behave like distinct characters rather than named copies of the same
assistant.

See [members](docs/members.md) for brief profiles and character sheets.




## Quick start

Install `rad`, `radicle-node`, and `radicle-team`, then prepare one member:

```sh
radicle-team setup nico --repo rad:...
```

After setup reports that Nico is ready, run him:

```sh
radicle-team run nico --repo rad:...
```

See [setup and operation](docs/setup.md) for complete installation, running, and
troubleshooting instructions.




## Documentation

See [`docs/`](docs/index.md) for guides, reference, and the team's own
documents.




## Development

- `make check` type-checks all features and targets.
- `make test-all` runs all tests and targets with `cargo-nextest`.
- `make lint` runs Clippy and verifies formatting.
- `make fmt` formats Rust code with nightly `rustfmt`.
- `make code-health` runs the extended health and analysis suite.

CI runs `check`, `test-all`, `lint`, and `code-health`.
