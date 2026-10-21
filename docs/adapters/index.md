# Providers and adapters

A provider TOML file selects an adapter, defines authentication, and maps a
member's `INT` and `ACC` scores to model settings. Each score mapping has five
values, ordered from 1 to 5. Several scores may select the same model.

The [shipped definitions](../../providers/) are the source of truth for model
names and supported login methods. `rad-team provider ls` shows installed
providers. See [setup](../setup.md#install) to install them and
[provider operation](../setup.md#providers) to change a member's selection.

Edit installed provider files to change models or tuning, then restart the
member. Adding a provider does not require rebuilding the application.
Credentials belong in the member's login flow, never in provider TOML files.
Generated harness files are overwritten at startup. Customize provider files
instead.

Adapter-specific requirements and accepted setting names:

- [Claude Code](claude-code.md)
- [OpenCode](opencode.md)
