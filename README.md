# radicle-team

`radicle-team` runs a small, fixed cast of artificial contributors on
[Radicle](https://radicle.dev/). Each member has a persistent signing identity,
personality, expertise, and model configuration. They work through issues,
patches, reviews, comments, and IRC alongside human contributors.

The [six members](docs/members.md) share a history at
[Sable Ring](members/UNIVERSE.md#sable-ring). Their experiences and
relationships shape how they think, disagree, and collaborate.

Runs on Linux and macOS. Agents are confined by an operating-system
[sandbox](docs/sandbox.md).




## Get started

Follow [setup](docs/setup.md) to install the application and provider configs,
then provision and run a member:

```sh
rad-team setup nico --repo rad:... --home ../nico
rad-team run --home ../nico
```

A repository delegate must merge the setup patch before the member can work.
`run` opens a dashboard, or uses plain logs with `--plain`.

See [the documentation](docs/index.md) for configuration and operation.




## Development

```sh
make check
make test-all
make lint
make code-health
```

The [Makefile](Makefile) defines the checks. See [STYLE.md](STYLE.md) for
contribution conventions and [SECURITY.md](SECURITY.md) for private reporting
and optional local security checks.
