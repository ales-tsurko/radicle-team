# Architecture

One `rad-team` process runs one member for one repository, with its own Radicle
identity, node, storage, and durable state. The canonical
[manifest](manifest.md) binds the member to a DID and grants permissions.




## Runtime and agent roles

| Role | Receives | Produces |
| --- | --- | --- |
| Talker | Repository activity | Replies and queued work |
| Coder | Durable tasks | Implementation patches and assigned reviews |
| Chatter | Authenticated IRC messages | Chat replies and queued work |

Roles run concurrently under the same identity. Each serializes its own work and
has a separate checkout, harness, and scoped MCP endpoint. Chatter starts only
when `[chat]` is configured. Every delivery starts a fresh agent session using
the member's documents and task context.

Adapters implement the [role interfaces](../src/agent/interface.rs), translate
provider settings, launch the agent, and report progress. Filesystem and network
restrictions are enforced by the [sandbox](sandbox.md).




## Delivery and recovery

The watcher admits authorized activity into a durable inbox. Assignment scans
continue while Talker is busy. Reviews and direct addresses precede ambient
changes, without interrupting the active turn. Reactions acknowledge queue
admission without running a model. Signed delivery records replicate through
Radicle for remote inspection.

With fresh state, conversation handling starts at the member node's startup
time. Older messages are skipped, including history fetched later. Open
assignments are still discovered regardless of age. Restarts retain that cutoff,
checkpoints, and unfinished work.

The roles share task specifications, artifact references, and working notes
through MCP. References do not grant write permission. Patch assignments request
reviews, and review tasks cannot publish implementation. Completion requires the
expected published result. Ready revisions return to a member with an
outstanding rejected review so it can recheck its own findings.

Failed agent calls retry with retained context. A stopped task is recovered on
restart. Cancellation asks the agent to preserve its work before stopping it.
The runtime also stops agents that report no activity for fifteen minutes.




## Workspaces

Role worktrees share one Git object store, while retaining separate working
trees, heads, and indexes. Coder uses a branch per task. Uncommitted work left
by an interrupted coding run is saved to a salvage branch before another task
starts. Talker and Chatter worktrees are cleaned between deliveries.

The runtime owns these worktrees and their branches. Git configuration, hooks,
worktree pointers, and other roles' references are protected. Runtime Git runs
inside its own sandbox because it handles repositories writable by agents.




## Chat

Each member connects outbound to an IRC server over pinned TLS. The server
asserts SASL accounts, which signed manifest attestations map to Radicle DIDs.
The runtime verifies the server, account binding, and trigger policy before
admitting a message. It does not authorize by nick.

Chat uses a separate role so a question can be answered during a long build or
review. The server remains trusted to assert accounts and can read messages. See
[chat](chat.md) for enrollment and privacy requirements.

The transport uses `irc-proto` over an application-owned TLS stream so it can
enforce the manifest's SPKI pin and client certificate.
