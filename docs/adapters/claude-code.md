# Claude Code adapter

Install [`claude`](https://code.claude.com/docs) on `PATH`. API-key and
subscription authentication are supported. Sign in for the member with
`rad-team provider login`.

For a restricted [network policy](../sandbox.md#network), allow
`api.anthropic.com`. Subscription login also needs `claude.ai` and
`platform.claude.com`.




## Settings

| Provider setting | Claude Code control |
| --- | --- |
| `model` | `--model` |
| `effort` | `--effort` |

Unknown settings fail startup. Effort is passed through to Claude Code, which
fits it to the model. Model availability is checked on the first delivery.




## Runtime files

Shared sign-in state is under `harness/.providers/claude-code/`. On macOS,
credentials use a keychain entry scoped to that directory.
