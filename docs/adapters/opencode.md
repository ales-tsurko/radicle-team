# The `opencode` adapter

What is specific to running a member through
[OpenCode](https://opencode.ai/docs). What every adapter does, and what the
runtime does around one, is in
[architecture](../architecture.md#runtime-and-agent-roles).




## What it needs

`opencode` on the `PATH` of the machine running the member. A role checks this
when it starts.

Network access to two places besides the provider's own API, both on the first
run under a member folder: `models.dev`, which is the registry OpenCode reads
model names and prices from, and `registry.npmjs.org`, which it installs its
plugin dependencies from. That first run therefore takes a minute or two. A
`network` list in [`sandbox.toml`](../sandbox.md#network) which omits them
leaves a member unable to start.




## Settings

A provider file names settings in whatever words reach the adapter best — these
are OpenCode's names. The crate reads them and hands them over as data, and this
adapter refuses any it does not carry before carrying the rest as OpenCode's own
controls, on an agent it defines for the member:

| Provider file | OpenCode |
| --- | --- |
| `model` | the agent's `model`, as `provider/model` |
| `reasoning_effort` | the agent's `variant` |
| `temperature` | the agent's `temperature` |
| `top_p` | the agent's `top_p` |

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

The model name is checked the same way. OpenCode lists a provider's models only
once it is authorized for that provider, so a name it does not list stops the
member at startup — a typo and a missing login look the same from here, and the
error says to check both.

Some models are hosted in China, which recent OpenCode asks you to opt in to
before it will offer them. That opt-in is in OpenCode, not here. Without it the
model reads as one OpenCode does not have.




## What it writes, and where

One `opencode.json` per role, in that role's harness folder, rewritten every
time the role starts. Besides the settings above it pins the things which keep a
headless run headless, none of which are yours to configure:

- `share` is `disabled`. A member's conversations belong to the repository, not
  to a web page on somebody else's host.
- `question` is denied. Nobody is on the other end of a delivery, so a question
  would hold the run open until the runtime stopped it for silence.
- `autoupdate` is off. Updating would rewrite the binary you installed, from
  inside a sandbox which denies exactly that.
- `snapshot` is off. The runtime owns this working copy's Git state.
- `small_model` is the member's own model, so the errands OpenCode would
  otherwise run on a cheap model of some other provider stay with the one that
  was authorized and paid for.
- `external_directory` is allowed, because the harness and the build cache are
  outside the working copy by design. The sandbox, not OpenCode, is what says
  where a member may write.

`HOME` points at the role's harness folder for the run, so OpenCode's settings,
sessions, credentials and logs land there — one folder per role, removable, and
never your own `~/.config` or the instructions of another agent application you
happen to use. Its log is under `harness/<role>/.local/share/opencode/log/`.

`XDG_CACHE_HOME` points at the member's `cache/` folder, which its three roles
share and which is safe to delete. The model registry download lands there, and
so does whatever a project's own toolchain caches under that variable. Pointing
a particular build tool at it is a project's business rather than the adapter's:
there is no cross-language variable for "build output goes here", and inventing
one for each ecosystem in here would only cover the ecosystems we thought of.




## Deliveries

One delivery is one `opencode run` in the role's working copy, with the message
saying what happened and everything else coming from the member's documents and
the MCP tools. Nothing is resumed between deliveries. The tools OpenCode takes
from the run-scoped MCP server appear to the model prefixed with the server's
name, so the reply tool for chat is `radicle_reply_chat`.

The adapter installs a plugin, `plugins/progress.js` under the role's harness
home, and OpenCode loads it on every run. The plugin appends one line per event
to `progress.jsonl` beside it, and the adapter reads that file to answer the
runtime's question of when the agent last did anything. Without it the runtime
would see nothing between the start of a run and its end, and a long tool call
would look the same as a hang.

The plugin is how a stalled permission prompt becomes visible too. It writes a
line when OpenCode asks for one, which from outside the process is
indistinguishable from an agent thinking.

A cancelled job is stopped and then preserved by a second, short run in the same
working copy, told to commit what the first one reached. An agent turn cannot be
interrupted, and OpenCode has no way to hand a running one a message, so the
work is preserved by a run which can see the tree rather than by the run which
made it. If that second run is stopped too, the changes are still in the tree
and preparing the next job commits them to a salvage branch.

`RUST_LOG=radicle_team=debug` adds every event a run emits, which is the
quickest way to see what a member is doing without reading OpenCode's own log.
