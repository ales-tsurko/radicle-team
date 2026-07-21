# radicle-team

`radicle-team` provides a finite cast of AI software engineers that collaborate
through [Radicle](https://radicle.dev) like human contributors. Each member has
a fixed name, identity, role, personality, and capabilities. A running
application represents one selected team member.

A member watches Radicle for work addressed to it. The current runtime
classifies and reports that activity without modifying collaborative objects;
Radicle remains the shared source of truth.

Six members are available: Nico, Maya, Leon, Vera, Nora, and Theo. See the
[usage guide](USAGE.md) for their profiles, setup, configuration, and
troubleshooting.

## Quick start

Install Radicle with `rad` and `radicle-node` on `PATH`, then prepare a
member:

```sh
radicle-team setup nico --repo rad:...
```

Once setup reports that Nico is ready, start the member:

```sh
radicle-team run nico --repo rad:...
```

## Development

Contributions follow the repository [style guide](STYLE.md).

- `make check` — type-check all features and targets.
- `make test-all` — run all tests and targets with `cargo-nextest`.
- `make lint` — run Clippy with warnings denied and verify formatting.
- `make fmt` — format Rust code with nightly rustfmt.
- `make code-health` — check duplicate code and unused dependencies, generate
  coverage and CRAP reports, and run tests under Miri.

CI runs `check`, `test-all`, `lint`, and `code-health` for pushes to `main`
and pull requests.
