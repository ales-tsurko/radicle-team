# Architecture

`rad-team` runs one selected team member as an isolated Radicle identity and
node. The same machine can run multiple members as separate processes.

Each member is an artificial contributor: a persistent identity that receives
work through ordinary Radicle activity, rather than a prompt run on demand.

This document covers how the runtime is put together and why it is shaped this
way. For operating instructions see [setup](setup.md), for configuration see
[the manifest reference](manifest.md).




## Runtime and agent roles

Each configured member receives an isolated Radicle home, signing identity,
node, repository storage, delivery workspace, and durable state. The
repository's reviewed `radicle-team.toml` manifest binds that member to its DID
and allowed tools.

A **role** is the kind of work one of a member's agents takes — three run
concurrently under its single Radicle identity, so one kind of work does not
wait behind another.

A member answers, chats, and builds at the same time, which one conversation
cannot do. A half-written patch has no safe point to pause at, and an agent turn
cannot be interrupted once it begins, so sharing one would put a question behind
a twenty-minute job. Each member therefore runs three agents under one identity,
one per role, with one MCP server each. To everybody reading the repository it
is one person.

`Talker` answers repository activity, `Chatter` answers chat, and `Coder`
carries out queued work. The chatting role is started only when the manifest has
a `[chat]` section, so a repository without chat runs two agents.

The runtime watches Radicle histories and converts relevant activity into typed
agent events:

- a mention, reply, or relevant change in a Radicle issue or patch goes to the
  member's talking agent;
- an assignment goes to the member's work queue, which the building agent takes
  jobs from;
- an authenticated chat message goes to the member's chatting agent, which may
  in turn record work in the queue when it decides work is needed;
- each agent runs inside a work-scoped workspace with a scoped MCP server, and
  what it publishes back is signed Radicle comments, issues, patches, reviews,
  or reactions.

Agent integrations implement the provider-neutral
[`Talker`](../src/agent/interface.rs), [`Chatter`](../src/agent/interface.rs),
and [`Coder`](../src/agent/interface.rs) interfaces. The runtime calls `start`
on each with the selected member's `Profile` and its own run-scoped MCP
endpoint, then delivers typed activity to the first, authenticated messages to
the second, and queued jobs to the third.

Four MCP tools connect the two. The talking agent records work with `add_job`,
checks `work_status`, corrects a running job with `note`, and stops it with
`cancel`. A note arrives on the building agent's next tool result, so nothing
polls, and cancelling asks it to commit what it has to a branch rather than
discarding it.

Each adapter is responsible for:

- Mapping provider-neutral tuning to model controls
- Supplying the member and shared Markdown documents as model context
- Managing the provider's conversation or execution runtime
- Using only the supplied workspace and allowed MCP tools
- Returning success only after the event or job has been handled durably

The core does not depend on a particular model vendor or agent framework.




## How chat reaches a member

Chat is a second ingress alongside the watcher, with its own dispatcher, its own
agent, and its own MCP scope. It does not share the talking agent's queue.

That separation is the whole point. An agent turn cannot be interrupted, so a
member that had started a review would leave a person in a channel waiting for
it to end. Prioritising chat within one queue only helps between turns, not
during one.

The cost is a third concurrent agent for this member whenever a message arrives.
A repository whose manifest has no `[chat]` section never pays it.

Each agent serialises its own work, so a member is at most one repository
delivery, one chat message, and one job at a time.

Nothing limits how long a delivery or a job may take, because reading a long
patch or building a real change legitimately takes a while. What is limited is
silence. Each adapter reports when it last saw its agent act, and one which has
shown nothing for fifteen minutes is stopped. A cancelled job is asked to commit
what it has, then killed thirty seconds later if it has not. Deciding that
belongs to the runtime, which is why the roles are internal to this crate rather
than an open interface.

A job which was running when the process died is still marked taken in the
queue, so it is put back at the front on the next start rather than left
silently lost.

Each member opens one outbound TLS connection to the team's IRC server and joins
the channels bound to its repository. Members run on separate machines, so each
connects on its own. Nothing listens for inbound connections and no member needs
a public address.

To everyone in the channel a member looks like any other user. Addressing one by
nick reaches it. Two members in the same channel are two users, the same way two
people are.

Nothing about the chosen server appears in the source. `rad-team` speaks IRC and
IRCv3, so any conforming server works. See [chat](chat.md) for what a server
must provide and how to deploy one.




## What chat trusts

The IRC server asserts accounts. If it lies, `account-tag` lies, and everything
built on the account-to-DID mapping lies with it. The team runs the server, so
this is trust in its own infrastructure rather than in a third party, but it is
real and it is worth stating rather than discovering later.

The alternative is signing each message inline with the sender's Radicle key.
That needs a Radicle-aware client and would end the ability to use the client
you already have, which is the entire reason for choosing an existing protocol.
The trade is deliberate.

Repository-wide MCP scope is created for a chat message only after the whole
chain succeeds: message tag to attestation to DID to policy. If any link
weakens, chat fails closed rather than falling back to narrower access. The
scope is safe because only attested accounts can address a member at all, unlike
issue text, which anyone can write. That dependency is the reason for the width,
so the two move together.

Cross-repository isolation is an existing property of the application. Chat does
not add its own.




## Rejected IRC alternatives

An earlier design proposed one bot per chat platform with a gateway routing
messages to members. It was dropped for several reasons.

It requires writing and deploying a router, which is what an IRC server already
is. It requires a per-platform adapter and tracking each platform's API
indefinitely. Chat identity would be a platform account rather than a Radicle
key, because an ordinary Telegram client cannot sign with a DID. A third party
would read every message. Whoever held the gateway could impersonate anyone in
the room.

Running one bot per member instead of a gateway does not survive contact with
the platforms. Telegram caps an account at twenty bots, which members times
repositories reaches at five members and four repositories. Telegram usernames
share one global namespace with all users and channels. Telegram bots cannot see
other bots' messages at all, so members could not share channel context. None of
it removes the platform from the trust path.

Building a chat application is out of scope. Real people talk in these channels
alongside the members and are the same kind of participant, which makes this
group chat: membership, shared history, offline delivery, several clients, and
mobile. That is a product rather than a feature.

The original objections to IRC were all about connecting to arbitrary
third-party servers, where nicks can be claimed while their owner is offline,
accounts must be provisioned on somebody else's machine, and the server cannot
verify Radicle keys. None of that applies to a server the team runs.

Matrix was reconsidered and rejected on weight. Its account and homeserver
objections do dissolve when you own the homeserver, and a Rust homeserver and
client library both exist, but state resolution, device management, and
encryption key handling are far more protocol than this needs.

Nostr remains rejected. Relay-based groups and MLS encrypted messaging are both
draft specifications, private group clients are few, and its keys are a separate
identity from the Radicle one in any case.

XMPP was never considered in the original design and is a real alternative. It
is lighter than Matrix with better mobile support than IRC, and its Rust
libraries are maintained. It loses to IRC on simplicity and has no Rust server
either.

Backing chat with collaborative objects is appealing and was examined closely.
It would delete the whole trust problem, since a message would be signed by the
DID that wrote it, removing the attestation format, the account mapping, and any
reliance on a server asserting identities. It would also reuse the watcher,
classification, and conversation limits rather than running a second ingress.

It fails on visibility. Radicle access control is repository-level only, either
public or private with an allowed set of DIDs, and there is no per-object or
per-type setting. Chat privacy would therefore equal repository visibility, so
on a public repository every message a team ever sent would be world-readable
and permanently archived. Fetching selectively does not help, because that lets
a reader opt out rather than letting a writer restrict access. Gossip latency
and the absence of any existing client are further costs on top.

Redaction is not a way out. Redacting sets the comment to `None` in the
materialized state and appends a new entry, leaving the original text in the
object's history where plain git still reads it. Editing behaves the same way. A
collaborative object is an append-only signed graph, so removing an entry would
break the signature chain and every replica. Peers who already fetched a message
keep it regardless.

SimpleX and building on Radicle gossip, Iroh, or libp2p remain rejected for the
reasons given when they were first considered.




## Why `irc-proto` rather than a high-level client

A spike proved that `irc::Client` exposes the required IRCv3 commands and tags
but cannot enforce the manifest's exact SPKI pin. In `irc` 1.1.0
[`src/client/conn.rs:336-381`][irc-connection], the crate privately constructs
its TLS configuration with WebPKI and native roots, optionally adds another
root, and exposes only an accept-all alternative. There is no custom verifier or
transport injection point.

`irc-proto::IrcCodec` preserves `account` and `msgid` tags over a caller-owned
stream, leaving DNS, TCP, client certificates, and strict SPKI verification
under this application's control.

[irc-connection]: https://docs.rs/crate/irc/1.1.0/source/src/client/conn.rs
