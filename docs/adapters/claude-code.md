# The `claude-code` adapter

What is specific to running a member through
[Claude Code](https://code.claude.com/docs). What every adapter does, and what
the runtime does around one, is in
[architecture](../architecture.md#runtime-and-agent-roles). The OpenCode
counterpart is [here](opencode.md), and where the two differ this page says so.




## What it needs

`claude` on the `PATH` of the machine running the member. A role checks this
when it starts, along with the authorization. A member given an API key places
the key in its own environment and asks Claude Code nothing; one authorized as a
subscription asks Claude Code itself what it is signed in as; and a member whose
provider was never recorded stops there rather than on its first delivery. A
provider which needs no authorization skips the question.

Network access to one place besides nothing: `api.anthropic.com`. Unlike
OpenCode there is no registry to download on a first run and no dependency
install, so a member starts as fast on its first delivery as on any other.
Telemetry, error reporting, feature-flag fetching and auto-updates are switched
off by the role itself, so nothing else on Anthropic's side is asked for either.
A `network` list in [`sandbox.toml`](../sandbox.md#network) naming only that
host — plus the two below when they apply — is enough.

A member authorized as a subscription also refreshes its sign-in through
`claude.ai` and `platform.claude.com`, so those belong on the list too.

Run `rad-team provider login` from the member directory to sign in once for all
roles. `CLAUDE_CONFIG_DIR` points at the member's shared
`harness/.providers/claude-code/` directory. On macOS, Claude Code uses a
keychain entry scoped to that configuration directory; on Linux, it stores
credentials in the directory.




## Settings

| Provider file | Claude Code |
| --- | --- |
| `model` | the run's `--model` |
| `effort` | the run's `--effort` |

Anything else is refused when the role starts. The error names the setting, the
settings the file gave, and the names this adapter could have used. Note that
the names are this adapter's, not shared: OpenCode carries the same effort
control under the name `reasoning_effort`, and a file written for one of the two
adapters is refused by the other rather than silently dropped. The values are
never touched.

The effort is sent exactly as the file asked for it. Where a model takes less,
Claude Code falls back to the nearest level at or below on its own — there is no
listing to fit against here, and no need: the fitting happens on the far side of
the flag.

There is no model catalogue either. OpenCode lists what a provider offers once
it is authorized, and stops a name it does not recognize at startup. Claude Code
offers no way to ask, so a name Anthropic will not serve turns up on the first
delivery instead, and the error says what went wrong. The provider files this
application ships name models chosen to work here, so there is nothing to check;
a wrong one can only arrive in a file you copied and edited yourself.




## What it writes, and where

Two files per role, both rewritten every time the role starts:

- `mcp.json` in the role's harness folder, naming the one MCP server the member
  reaches. Its tools appear to the model prefixed with the server's name, so the
  reply tool for chat is `mcp__radicle__reply_chat`.
- `instructions/ALL.md` inside the role's instructions directory — the six
  documents instructing the member combined into the single standing prompt one
  delivery takes, in the order the documents themselves establish. It lives
  beside them because that directory is the one thing in the harness home the
  sandbox keeps read-only to the agent, and an instruction file an agent could
  rewrite would not stay an instruction for long.

`HOME` points at the role's harness folder. `CLAUDE_CONFIG_DIR` points at the
member's shared `harness/.providers/claude-code/` directory so all roles use one
sign-in. Runtime MCP settings and instructions stay separate per role.
`XDG_CACHE_HOME` points at the member's `cache/` folder, which its three roles
share and which is safe to delete.

Besides the settings above, every delivery runs under pins none of which are
yours to configure:

- permissions are bypassed outright. Nothing can stop to ask a person before
  acting; the sandbox, not a permission system nobody can answer, is what
  confines the run.
- only this harness home's own settings load. A repository's `.claude/` folder
  would otherwise contribute hooks and settings to a session run in it, and a
  member works on repositories somebody else controls.
- the MCP configuration above is the only one consulted, whatever else the
  machine offers.
- sessions are not persisted. Nothing is resumed between deliveries, so nothing
  is even kept to resume.
- updates, telemetry, error reporting and feature-flag fetching are off.
  Updating would rewrite the binary you installed from inside a sandbox which
  denies exactly that, and the rest is traffic a member has no use for.
- the errand model — the cheaper one Claude Code spends on naming things and
  choosing tools — is pinned to the member's own model, so those requests stay
  with the account that was authorized and paid for.




## Deliveries

One delivery is one `claude -p` in the role's working copy, with the message
saying what happened and everything else coming from the member's documents and
the MCP tools. Each event a run emits arrives on its output as it happens, and
that stream is the whole progress story: every line is one more thing the agent
just did, and the runtime reads silence the same way it does for OpenCode, only
without the plugin that harness needs. A failure explains itself in the final
event, which the runtime relays when a delivery ends badly.

A cancelled job works the way it does under OpenCode, because the reason it does
is the same: an agent turn cannot be interrupted. The stopped job is ended, and
a second, short run in the same working copy commits what the first reached. If
that second run is stopped too, the changes are still in the tree and preparing
the next job commits them to a salvage branch.

`RUST_LOG=radicle_team=debug` adds every event a run emits, which is the
quickest way to see what a member is doing without reading Claude Code's own
logs.
