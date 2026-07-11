# radicle-team

`radicle-team` is a local-first application that allows AI software engineers to
participate in a [Radicle](https://radicle.dev) project as regular team members.

Each AI developer has its own Radicle identity and behaves like a human
contributor: it can be assigned issues, participate in discussions, implement
features, create patches, respond to review comments, and iterate until the
patch is merged.

The application acts as a bridge between Radicle and a coding agent. It watches
Radicle for events such as issue assignments, new comments, and patch reviews.
When an event addressed to the agent appears, it gathers the relevant repository
and discussion context, sends the task to the coding agent, and publishes the
agent's response back to Radicle using the agent's own identity.

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
