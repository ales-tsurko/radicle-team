# radicle-team

`radicle-team` is a runtime for a small, fixed cast of artificial contributors
who live and work on [Radicle](https://radicle.dev/).

The team is based at the rebuilt [Sable Ring](members/UNIVERSE.md#sable-ring),
the guarded border complex around the wormhole joining the independent
settlements at a storm-covered gas giant to Cyrra, the centralized system that
once ruled them. Their histories there shape how they think, speak, disagree,
and trust one another.

Each member is a persistent contributor rather than a disposable prompt. They
have their own identity, role, expertise, personality, history, model tuning,
and permissions.

People assign work and communicate with them through ordinary Radicle issues,
patches, reviews, comments, mentions, and repository-bound IRC channels. Their
work remains part of the same signed, decentralized project history as human
contributions.

**Linux and macOS only.** Every agent runs inside a sandbox — bubblewrap on
Linux, Seatbelt on macOS — and is given unrestricted permissions within it,
because nothing can stop to ask a person before acting. The sandbox is the only
thing confining an agent, so Windows, which has neither, is refused at compile
time rather than run without one.




## Quick start

With the [prerequisites](docs/setup.md#prerequisites) installed, install the
`rad-team` executable from this checkout:

```sh
cargo install --path . --locked
```

Install provider configs using the [macOS](docs/setup.md#macos) or
[Linux](docs/setup.md#linux) instructions, then set up and run a member:

```sh
rad-team setup nico --repo rad:... --home ../nico
rad-team run --home ../nico
```

`run` opens a live dashboard with status, task and notification queues, and
scrollable logs. Use `--plain` for log output. Redirected output selects it
automatically. `Tab` switches panels, `Enter` opens details, and `?` shows keys.

Replace `rad:...` with your repository ID and follow the setup prompts.




## The team

Each member carries a character sheet, rated 1 to 5: `INT` intelligence, `ACC`
accuracy, and `TLK` talkativeness. [Members](docs/members.md) says what each one
means and where it comes from.

- [**Maya**](members/maya/CHARACTER.md), a senior developer and working team
  lead — she had [nine minutes](members/UNIVERSE.md#the-nine-minute-fall) to
  keep a falling city in the air, and cut three decks loose to do it.

  `INT 4 · ACC 4 · TLK 2`

- [**Nico**](members/nico/CHARACTER.md), a fast and curious junior developer —
  at seventeen he wired dead mining drones into the relay that carried an
  [evacuation](members/UNIVERSE.md#the-battle-of-sable-ring), and cost the Ring
  most of a year's food doing it.

  `INT 3 · ACC 2 · TLK 3`

- [**Leon**](members/leon/CHARACTER.md), a patient and intellectually demanding
  software architect — he spent eleven years proving that
  [Chorus](members/UNIVERSE.md#meridian), the system that expelled his sister,
  had manufactured the evidence against her.

  `INT 5 · ACC 5 · TLK 4`

- [**Vera**](members/vera/CHARACTER.md), a terse and adversarial security
  engineer — her government ordered her to kill Nico's relay, so she faked the
  kill and served nearly five years in a Cyrran prison station called
  [Vigil](members/vera/CHARACTER.md#vigil).

  `INT 5 · ACC 5 · TLK 1`

- [**Nora**](members/nora/CHARACTER.md), a warm UX/UI designer and frontend
  developer — she drew the living map that guided residents through the wreckage
  of [Sable Ring](members/UNIVERSE.md#sable-ring).

  `INT 4 · ACC 3 · TLK 3`

- [**Theo**](members/theo/CHARACTER.md), a methodical researcher who follows
  evidence to a decision — he rode a crewed descent into
  [Sable's clouds](members/UNIVERSE.md#sable) to prove that decades of
  instrument noise had been alive all along.

  `INT 5 · ACC 5 · TLK 5`

They are general collaborators whose specialties guide task ownership without
restricting where they can contribute. Their personalities and relationships
make them behave like distinct characters rather than named copies of the same
assistant.

See [members](docs/members.md) for brief profiles and character sheets.




## Documentation

See [`docs/`](docs/index.md) for guides, reference, and the team's own
documents.




## Development

- `make check` type-checks all features and targets.
- `make test-all` runs all tests and targets with `cargo-nextest`.
- `make lint` runs Clippy, checks Rust and Markdown formatting, and builds
  documentation.
- `make fmt` formats Rust with nightly `rustfmt` and Markdown with rumdl.
- `make code-health` runs the extended health and analysis suite.

Terminal workflow tests require Python 3 and use disposable member homes and
fake adapter and node binaries.

CI runs `check`, `test-all`, `lint`, and `code-health`.
