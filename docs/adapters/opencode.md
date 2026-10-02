# The `opencode` adapter

What is specific to running a member through
[OpenCode](https://opencode.ai/docs). What every adapter does, and what the
runtime does around one, is in
[architecture](../architecture.md#runtime-and-agent-roles).




## What it needs

OpenCode 2 on `PATH`. Tested with 2.0.13.

Allow the provider's API and `models.dev` in the
[`sandbox.toml` network list](../sandbox.md#network). Providers or plugins
installed from npm also need `registry.npmjs.org`.




## Settings

The adapter maps provider settings to the member's OpenCode agent:

| Provider file | OpenCode |
| --- | --- |
| `model` | the agent's `model`, as `provider/model` |
| `reasoning_effort` | a `#variant` suffix on the selected model |
| `temperature` | the provider model's `body.temperature` |
| `top_p` | the provider model's `body.top_p` |

Anything else is refused when the role starts. The error names the setting, the
settings the file gave, and the names it could have used. The values are never
touched.

A model's efforts come from the registry rather than from us, and two models of
one provider rarely offer the same ones — a ladder can even climb from a model
which does not reason to one which does. So the score is fitted to the model it
lands on: the effort itself when that model offers it, otherwise the nearest one
below it, otherwise the least that model will do, and nothing at all for a model
with no efforts, which then runs at its own default. Each of those is logged
with what was asked for and what was sent.

The model name is checked at startup. A missing model or provider authorization
stops the member and reports what to check.

Some models are hosted in China, which recent OpenCode asks you to opt in to
before it will offer them. That opt-in is in OpenCode, not here. Without it the
model reads as one OpenCode does not have.

The adapter queries models through its plugin RPC, which waits for providers to
finish loading.




## What it writes, and where

One `opencode.json` per role, in that role's harness folder, rewritten every
time the role starts. Besides the settings above it pins the things which keep a
headless run headless, none of which are yours to configure:

- `share` is `disabled`. A member's conversations belong to the repository, not
  to a web page on somebody else's host.
- `permissions` deny `question` for every resource. Nobody is available to
  answer during a delivery.
- `update` is `disable`. Updating would rewrite the binary you installed, from
  inside a sandbox which denies exactly that.
- `snapshots` is off. The runtime owns this working copy's Git state.
- `agents.title.model` is the member's own model, so title generation uses the
  same authorized provider.
- `external_directory` is allowed, because the harness and the build cache are
  outside the working copy by design. The sandbox controls where a member may
  write.

`HOME` points at the role's harness folder, keeping its configuration separate.
`XDG_DATA_HOME` points at `harness/.providers/opencode/data/` inside the member
home. OpenCode keeps native credentials, sessions, and logs in that shared data
store; each delivery still starts a fresh session. Its log is under
`harness/.providers/opencode/data/opencode/log/`. Run `rad-team provider login`
from the member directory to sign in once for all roles.

`XDG_CACHE_HOME` points at the member's `cache/` folder, which its three roles
share and which is safe to delete. The model registry download lands there, and
so does whatever a project's own toolchain caches under that variable. Pointing
a particular build tool at it is a project's business rather than the adapter's:
there is no cross-language variable for "build output goes here", and inventing
one for each ecosystem in here would only cover the ecosystems we thought of.




## Deliveries

One delivery is one `opencode run` in the role's working copy, with the message
saying what happened and everything else coming from the member's documents and
the MCP tools. Each run selects the member's model and fitted variant with
`--model` and uses `--standalone` to keep its server inside the sandbox. Nothing
is resumed between deliveries.

MCP tools are available through `execute`, as `tools.radicle.reply_chat(...)`.

The adapter installs a plugin, `plugins/progress.js` under the role's harness
home. Its event subscription appends live events to `progress.jsonl`, which the
runtime uses to detect stalled runs even when CLI output is buffered.

The plugin is how a stalled permission prompt becomes visible too. It writes a
line when OpenCode asks for one, which from outside the process is
indistinguishable from an agent thinking.

A cancelled job is stopped and then preserved by a second, short run in the same
working copy, told to commit what the first one reached. If that second run is
stopped too, the changes stay in the tree and preparing the next job commits
them to a salvage branch.

`rad-team run --log debug` adds every event a run emits, which is the quickest
way to see what a member is doing without reading OpenCode's own log.
