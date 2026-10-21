# Repository manifest

`radicle-team.toml` defines shared identities and policy. The runtime reads the
canonical branch by default. `--manifest PATH` replaces it with a local file,
without merging the two. Changes require a restart. The delegate set is checked
on every scan.

Keep credentials and private keys outside this file.

```toml
version = 1
triggered_by = "delegates"       # optional, shown default

[security]                       # required
repository = "rad:z3Security..." # private report repository
recipients = ["age1..."]         # one or more age X25519 recipients

[priority]                       # optional, shown defaults
front = "urgent"
next = "next"

[limits]                         # optional, shown default
exchanges = 3

[members.nico]
signing_did = "did:key:z6Mk..."  # required
generation = 1                   # required, must not decrease
state = "active"                 # required, or "inactive"
mcp_allow = ["read", "comments"] # required, may be empty
provider = "gpt"                 # optional, installed provider name
receipt = "🔧"                   # optional, Radicle-supported emoji
```

Each member has its own table. `provider` selects a local provider definition,
which maps character scores to models. `rad-team run --provider NAME` overrides
it for that process.

`receipt` acknowledges durable queue admission. It requires `set_reaction` and
has a member-specific default. Signed delivery records remain available without
reaction permission.




## Who may put a member to work

`triggered_by` accepts `"delegates"` (the default), `"anyone"`, or a list of
DIDs. Delegates are always allowed. On a public repository, `"anyone"` lets
other peers spend the operator's model budget.

A delegate's review reaches the member owning the patch without a mention. Other
people's activity follows `triggered_by`. A member never triggers itself, and
reactions never trigger work. Request work in a comment or assignment.




## What members may trigger in each other

Members may assign work, mention another member directly, and address its
comments or reviews regardless of `triggered_by`. Submitted reviews and ready
revisions also reach their relevant owners. Ambient changes and `@team` do not
trigger peers.

`limits.exchanges` bounds conversations per pair of members on each issue,
patch, or channel. Consecutive operations by one author count as one turn. An
authorized person assigning, mentioning, or replying to a member resets the
count. Unrelated changes do not.




## Which tools a member may use

`mcp_allow` accepts exact MCP method names and these groups: `read`, `chat`,
`comments`, `issues`, `patches`, `reactions`, and `reviews`. An empty list
grants no tools controlled by this list. `merge_patch` requires explicit
permission, even with `patches`. `comment_issue` belongs to `issues`.

Role restrictions still apply: chat replies belong to Chatter, patch creation
and updates to Coder, and queue management to Talker and Chatter. Queue tools
and `report_security_issue` have separate policy and cannot be named in
`mcp_allow`. MCP `tools/list` gives the available methods.




## Chat

Omit `[chat]` to disable it. See [chat](chat.md) for server requirements and
participant enrollment.

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
signature = "..."
```

Channel keys are logical identifiers, and values are IRC channels. Accounts and
channels use a lowercase portable subset. Accounts, DIDs, certificates, and
channel targets must be unique. Every active member needs a valid attestation.

Pins contain 64 lowercase hexadecimal digits. `spki-sha256:` hashes the server
certificate's DER SubjectPublicKeyInfo. `cert-sha256:` hashes the complete DER
client certificate.

Each participant signs this UTF-8 payload with their Radicle key, including its
final newline. Store the Ed25519 signature as a hexadecimal string:

```text
radicle-team-chat-attestation-v1
repository=rad:z3cyotNHuasWowQ2h4yF9c3tFFdvc
server=ircs://irc.example.com:6697
server-pin=spki-sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
account=nico
client-certificate=cert-sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789
did=did:key:z6Mk...
```

The self-signature proves the DID owner accepted the account binding. Manifest
review alone cannot substitute for it.




## Where new work goes

Tasks with the `priority.front` label precede `priority.next`, then normal work.
Within a priority, tasks run in arrival order. A higher-priority arrival does
not interrupt the current task. If both labels occur, `front` wins.




## Security reports

`security.repository` must be private and separate from the watched repository.
At least one valid age X25519 recipient is required. Every member can call
`report_security_issue`, regardless of `mcp_allow`. It encrypts the report and
files a generically titled issue. Members cannot read the report repository.

Keep report decryption keys and the operator's Radicle profile outside agent
environments.
