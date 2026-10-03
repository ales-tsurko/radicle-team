# Repository manifest

Exactly one `radicle-team.toml` file is active. The default is the file on the
repository's canonical branch. Use `--manifest PATH` to select an explicit file
with the same schema instead. The two sources are never merged.

During interactive setup, the wizard asks before updating an explicit manifest
at `PATH`, creating it when absent. It updates that file directly and does not
create a repository patch. New explicit manifests use mode `0600` on Unix.

The repository manifest defines shared policy: every contributor sees the same
reviewed mapping between members and DIDs. Machine-private identity material,
logs, locks, and watcher state stay in the member's own folder.

```toml
version = 1                              # required, must be 1
triggered_by = "delegates"               # optional, defaults to "delegates"

[security]                               # required
repository = "rad:z3Security..."         # private repo for reports
recipients = ["age1..."]                 # age X25519, at least one

[priority]                               # optional, both keys optional
front = "urgent"                         # defaults to "urgent"
next = "next"                            # defaults to "next"

[limits]                                 # optional
exchanges = 3                            # defaults to 3

[members.nico]                           # one table per member, fixed names
signing_did = "did:key:z6Mk..."          # required
generation = 1                           # required, monotonic
state = "active"                         # required, "active" or "inactive"
mcp_allow = ["read", "comments"]         # required, may be empty
provider = "gpt"                         # optional, a name from `rad-team provider ls`
```

`provider` names which models a member runs on, never a model itself: which
model of that provider it gets is decided from the member's character sheet by
that provider's own configuration, which is machine-local. Whoever runs the
member holds its credentials and pays for it, so `rad-team run --provider NAME`
overrides what the manifest says.

Every value here is read once at startup, so changing one needs a restart. The
single exception is the delegate set, which is re-read on every scan: granting
or revoking a delegate takes effect immediately.




## Who may put a member to work

`triggered_by` decides whose activity may put a member to work. Assignments,
mentions, replies, and other changes from an identity outside it are recorded
but never dispatched, so they cost nothing.

```toml
triggered_by = "delegates"                    # default when omitted
triggered_by = "anyone"
triggered_by = ["did:key:z6Mk...", "did:key:z6Mk..."]
```

Repository delegates are always allowed, including when explicit DIDs are
listed, because they control this manifest and could grant themselves access by
editing it. `"anyone"` suits a private repository, where Radicle's own access
list already decides who can write. On a public repository it lets any seeding
peer spend the operator's agent budget.

A delegate's review reaches the member who owns the patch without a mention,
including a verdict with no summary. Reviews from other people follow
`triggered_by`; with the default, unauthorized reviews never run an agent.

The setup wizard asks for the value when it creates a manifest and leaves it
alone when it later adds a member, so editing it by hand is safe.

Reactions never trigger anyone, whoever adds them. Approving an idea and asking
for it to be carried out look identical, so acting on a reaction risks work
nobody asked for. Request work in a comment.

A member's own activity never triggers itself.




## What members may trigger in each other

Members are always allowed, whatever `triggered_by` says, so that one can
delegate to another without being listed. What they may trigger in each other is
narrower than what a person may, so that a discussion between them cannot feed
itself while nobody is watching:

- assigning work to a member,
- naming one directly, as `@nico`,
- replying to a comment that member owns.

`@team` is not among them, so no member can summon the whole cast, and neither
are ambient changes such as an edited title. A submitted review and a new
revision are the exceptions: those carry work to whoever owns the subject, and
without them delegated work would stall once it was reviewed.

Two members also stop talking to each other after a while. A conversation
between them runs for a fixed number of turns on one issue, patch, or channel,
then waits for a person. Without it, two members can review and revise the same
patch forever, because neither act costs either of them anything to repeat.

```toml
[limits]
exchanges = 3
```

A turn is a run of operations by one author, so a review and the fifteen line
comments it is made of cost what one remark costs. Turns are counted per pair,
so a patch with three reviewers gives each of them a whole conversation rather
than a third of one, and what a member writes never spends its own allowance.

The count returns to zero when a person addresses the member: assigning it,
naming it, or replying to something it wrote. Adding a label or commenting to
somebody else does not, so nobody restarts a stalled thread by touching the
patch for an unrelated reason.




## Which tools a member may use

`mcp_allow` accepts exact MCP method names and the fixed `read`, `chat`,
`comments`, `issues`, `patches`, `reactions`, and `reviews` groups. `chat`
contains `reply_chat`. The `patches` group does not include `merge_patch`, which
has to be allowed by name. `mcp_allow` is required, while an empty list disables
every tool.

Groups are derived from the tools registered by the MCP server. `get_*` and
`list_*` tools are reads, and the rest are grouped by the `comment`, `issue`,
`patch`, `reaction`, or `review` component in their name. MCP `tools/list` is
the authoritative method list. `comment_issue` belongs only to `issues`.

Five tools sit outside `mcp_allow` entirely, and naming one there is rejected:
`report_security_issue`, which `[security]` below controls and every member has,
and the four queue tools, which reach only the member's own queue and never the
repository.

The three roles then divide what is left. The queue tools go to the talking and
chatting roles, since both decide that work is needed. `reply_chat` goes to the
chatting role alone, because only it holds a message to answer. `create_patch`
and `update_patch` go to the building role alone, since they need commits the
other two cannot make. Everything else follows `mcp_allow` on all three.




## Chat

Chat is optional and omitting `[chat]` disables it. When present, the section
names the repository again so every signed identity binding is
repository-specific:

```toml
[chat]
repository = "rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc"

[chat.server]
endpoint = "ircs://irc.example.com:6697"
pin = "spki-sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"

[chat.channels]
team = "#project"
security = "#project-security"

[[chat.participants]]
account = "nico"
certificate = "cert-sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
did = "did:key:z6Mk..."
signature = "z..."
```

The left side of `[chat.channels]` is the logical channel identifier. The right
side is the IRC channel. Account names and IRC channels use a lowercase portable
subset so matching never depends on a server's casemapping.

Every active member must be attested. People may be attested too, but their DID
must still pass `triggered_by` or delegate policy. Incoming nicks are never used
for authorization: only an attested `account-tag` received over the pinned
server can create authenticated chat work.


### Fingerprint encodings

`spki-sha256:` is followed by exactly 64 lowercase hexadecimal digits: SHA-256
of the server certificate's DER-encoded SubjectPublicKeyInfo. This survives a
certificate renewal that keeps the key. `cert-sha256:` has the same encoding but
hashes the complete DER-encoded client certificate. The latter is enrollment
evidence, not something IRC peers inspect at runtime.


### Attestation payload

The signature is the Radicle Ed25519 signature in its normal multibase form. It
covers this exact UTF-8 payload, including the final LF:

```text
radicle-team-chat-attestation-v1
repository=rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc
server=ircs://irc.example.com:6697
server-pin=spki-sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
account=nico
client-certificate=cert-sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789
did=did:key:z6Mk...
```

Each DID signs its own binding. Manifest review authorizes the participant, but
the self-signature separately proves that the DID owner accepted this IRC
account and certificate. A rogue or compromised delegate therefore cannot bind
Mallory's IRC account to Alice's DID and inherit Alice's permissions merely by
editing the manifest.

Parsing verifies every signature, rejects duplicate accounts, DIDs, certificates
and IRC channel targets, and requires an attestation for every active member.
Runtime authentication additionally requires the active RID and observed server
pin to match before an `account-tag` can produce a sender DID.




## Where new work goes

An assignment becomes a task in the member's queue. Waiting tasks run in this
order: `urgent`, then `next`, then tasks without either label. Within each
priority, tasks run in arrival order. New assignments do not interrupt the
running task. If both labels are present, `urgent` wins.

```toml
[priority]
front = "urgent"
next = "next"
```

The section is optional and only renames the two labels, for a repository that
already uses different words. Omit it and the defaults above apply.




## Security reports

`[security]` names a second repository, separate from the one the member
watches. It must be private, and both it and at least one valid age X25519
recipient are required.

Members cannot read it. It is never watched and none of its read operations are
exposed, so a member can write to it and nothing more. Writing is
`report_security_issue`, which encrypts the whole report and files it as a
generically titled issue.

Every member has that tool whatever `mcp_allow` says, including a member with no
issue permission at all. A member that finds something dangerous must always be
able to report it, so this is not left to configuration.

Agent adapters receive only the scoped MCP endpoint. Trusted adapter code must
also keep the host Radicle profile and recovery keys outside model subprocesses.
