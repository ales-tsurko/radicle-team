# OpenCode adapter

Install [OpenCode 2](https://opencode.ai/docs) on `PATH`. Tested with 2.0.13.

For a restricted [network policy](../sandbox.md#network), allow the provider's
API and `models.dev`. Providers or plugins installed from npm also need
`registry.npmjs.org`. Some providers require opting in through OpenCode before
their models become available.




## Settings

| Provider setting | OpenCode control |
| --- | --- |
| `model` | Model name as `provider/model` |
| `reasoning_effort` | Model variant |
| `temperature` | `body.temperature` |
| `top_p` | `body.top_p` |

Unknown settings fail startup. Reasoning effort uses the requested variant when
available, otherwise the nearest lower variant or the model's minimum. Models
without variants use their default. Model availability and authorization are
checked at startup.




## Runtime files

Shared credentials, sessions, and logs live under
`harness/.providers/opencode/data/opencode/`. Sign in with
`rad-team provider login`. Provider logs are in that directory's `log/` folder.
