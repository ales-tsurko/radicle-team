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
patches, reviews, comments, mentions, and reactions. Their work remains part of
the same signed, decentralized project history as human contributions.




## The team

- **Nico**, a fast and curious junior developer
- **Maya**, a senior developer and working team lead
- **Leon**, a patient and intellectually demanding software architect
- **Vera**, a terse and adversarial security engineer
- **Nora**, a warm UX/UI designer and frontend developer
- **Theo**, a methodical researcher who follows evidence to a decision

They are general collaborators whose specialties guide task ownership without
restricting where they can contribute. Their personalities and relationships
make them behave like distinct characters rather than named copies of the same
assistant.

See the [usage guide](USAGE.md) for brief profiles and tuning values.




## Quick start

Install `rad`, `radicle-node`, and `radicle-team`, then prepare one member:

```sh
radicle-team setup nico --repo rad:...
```

After setup reports that Nico is ready, run him:

```sh
radicle-team run nico --repo rad:...
```

See the [usage guide](USAGE.md) for complete setup, configuration, and
diagnostic instructions.




## Documentation

- [Usage guide](USAGE.md)
- [Shared team rules](members/TEAM.md)
- [Fictional universe](members/UNIVERSE.md)
- [Contribution style](STYLE.md)

Individual character, expertise, and voice documents live under
[`members/`](members/).




## Development

- `make check` type-checks all features and targets.
- `make test-all` runs all tests and targets with `cargo-nextest`.
- `make lint` runs Clippy and verifies formatting.
- `make fmt` formats Rust code with nightly `rustfmt`.
- `make code-health` runs the extended health and analysis suite.

CI runs `check`, `test-all`, `lint`, and `code-health`.
