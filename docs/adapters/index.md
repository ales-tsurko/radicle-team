# Adapters and providers

An adapter runs a member on one agent application. A provider file names the
adapter it is reached through, so choosing a provider chooses both the models
and the application that talks to them. What an adapter must do is in
[architecture](../architecture.md#runtime-and-agent-roles).




## What ships

`rad-team providers` prints the same list on your own machine, and adds whether
each one is authorized. The models are listed from the one a member scored 1
runs on to the one a member scored 5 runs on.

|Provider|Adapter|Models|ACC|Authorized with|
|---|---|---|---|---|
|`anthropic`|`claude-code`|Sonnet 5, Opus 5, Fable 5|effort|key or subscription|
|`deepseek`|`opencode`|V4 Flash, V4 Pro|effort|key|
|`glm`|`opencode`|GLM-4.7, GLM-5 Turbo, GLM-5.2, GLM-5.3|temperature|key|
|`gpt`|`opencode`|GPT-5.6 Luna, Terra, Sol|effort|key or subscription|
|`grok`|`opencode`|Grok 4.3, Grok 4.5|effort|key|
|`kimi`|`opencode`|K2.5, K2.7 Code, K3|effort|key|
|`ollama`|`opencode`|qwen3.5 9b, 27b, 35b|temperature|nothing|
|`qwen`|`opencode`|Qwen3.6 Flash, 3.7 Plus, 3.8 Max|temperature|key|

Each file says why its ladder is the shape it is, and which environment variable
holds its key. Read the one you are about to use; they are short. A provider we
do not ship is a file you write, for whichever adapter suits it. See
[choosing models](../setup.md#choosing-models).

A member runs on the provider its manifest binding names, so moving a member
between providers is one name in the manifest, or `--provider` on the command
line.




## Adapters

These pages cover what is specific to one agent application: what has to be
installed, which of a member's scores it can carry, and what it needs beyond the
provider's own API.

- [`claude-code`](claude-code.md)
- [`opencode`](opencode.md)
