# radicle-team

`radicle-team` runs a predefined team of AI software engineers that collaborate
through [Radicle](https://radicle.dev) like human contributors. Each member has
its own Radicle identity, role, seniority, personality, model, and capabilities.
Teams can mix fast, inexpensive junior developers with more capable senior,
architecture, security, or other specialist agents.

An agent watches Radicle for work addressed to it: issue assignments, mentions,
replies, and patch reviews. It can discuss and create issues, implement assigned
work, publish patches, and address review comments until the patch is accepted.
Radicle remains the shared source of truth for code and collaboration.

The project is distributed as a single binary. Each team member runs as a
separate configured process with its own Radicle node and coding-agent backend.
The whole team can run on one local machine or be distributed across servers;
members coordinate only through Radicle.

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
