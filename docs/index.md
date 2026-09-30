# Documentation

`radicle-team` runs a small, fixed cast of artificial contributors as isolated
Radicle identities. Start with [setup and operation](setup.md) to get one
running, or [architecture](architecture.md) to understand how it is put
together.




## Guides

- [Setup and operation](setup.md) — prerequisites, provisioning a member,
  running one or several, command-line options, local state, troubleshooting.
- [Architecture](architecture.md) — the three roles behind one identity, how
  activity and chat reach a member, what chat trusts, and why the alternatives
  were rejected.
- [Members](members.md) — the roster, their character sheets, and how to address
  them.
- [Adapters and providers](adapters/index.md) — which models ship, which agent
  application reaches them, and what each of those needs.




## Reference

- [Repository manifest](manifest.md) — the complete `radicle-team.toml` schema,
  including who may trigger a member, tool permissions, conversation limits,
  chat identity, and security reports.
- [Local sandbox](sandbox.md) — the administrator-owned `sandbox.toml`,
  filesystem and network policy, and platform behaviour.
- [Chat](chat.md) — what an IRC server must provide, how accounts map to DIDs,
  channel binding, privacy, and deploying a server.
- [The `opencode` adapter](adapters/opencode.md) — what it needs, which settings
  it carries, and what it writes where.
- [Ergo deployment](../deploy/ergo/README.md) — the recommended Compose
  deployment.




## The team itself

- [Shared team rules](../members/TEAM.md) — the instructions every member
  follows.
- [The Sable system](../members/UNIVERSE.md) — where they come from and what
  happened there.
- Role-specific instructions — [`TALKER.md`](../members/TALKER.md) for issues
  and patches, [`CHATTER.md`](../members/CHATTER.md) for chat, and
  [`CODER.md`](../members/CODER.md) for building. Each agent is given the one
  for the role it runs.

Individual character, expertise, and voice documents live under
[`members/`](../members/).




## Contributing

- [Contribution style](../STYLE.md) — how changes to this repository are
  written.
