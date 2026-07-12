# radicle-team

`radicle-team` provides a finite set of AI software-engineer presets that
collaborate through [Radicle](https://radicle.dev) like human contributors. Each
preset is a stable character with a fixed name, identity, role, personality, and
capabilities. A running application represents one selected team member.

An agent watches Radicle for work addressed to it: issue assignments, mentions,
replies, and patch reviews. It can discuss and create issues, implement assigned
work, publish patches, and address review comments until the patch is accepted.
Radicle remains the shared source of truth for code and collaboration.

The project provides a reusable library and a thin binary. Each selected member
runs as a separate process with its own Radicle node and identity. The whole
team can run on one local machine or be distributed across servers; members
coordinate only through Radicle.

## Development

- `make run` — run the application.
- `make check` — type-check all features and targets.
- `make test-all` — run all tests and targets with `cargo-nextest`.
- `make lint` — run Clippy with warnings denied and verify formatting.
- `make fmt` — format Rust code with nightly rustfmt.
- `make code-health` — check duplicate code and unused dependencies, generate
  coverage and CRAP reports, and run tests under Miri.

CI runs `check`, `test-all`, `lint`, and `code-health` for pushes to `main` and
pull requests.
