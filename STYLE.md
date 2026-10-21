# Style guide

Follow the [Rust Style Guide](https://doc.rust-lang.org/style-guide/) and
[rustc conventions][rustc-style]. Nightly `rustfmt` is the Rust formatting
authority.




## Module layout

Keep `lib.rs` and `mod.rs` focused on documentation, re-exports, and
declarations. Keep small glue there only when extraction would make navigation
harder. Modules with submodules use `foo/mod.rs` and `foo/submodule.rs`.

Order each module as follows:

1. Module documentation, imports, re-exports, module declarations, constants.
2. The primary type followed by its inherent and trait implementations.
3. Supporting types in introduction order, each followed by its implementations.
4. Private utilities, error types, tests.

Imports follow `std`/`core`, third-party, then current-crate order.
Current-crate paths start with `crate::`. Move items earlier only when lexical
rules require it, such as a macro used by the primary implementation.




## Functions and types

- Prefer methods on the type owning the state or context. Standalone functions
  suit operations without a natural owner, `main`, tests, and trait
  requirements.
- Use traits and conversions such as `From` to clarify extension points and
  localize variant-specific logic.
- Derive errors with `thiserror`.




## Formatting and documentation

Run `make fmt` before `make lint`. Formatting uses nightly `rustfmt` and rumdl
0.2.75 with `.rumdl.toml`. For one Markdown file, use `rumdl fmt PATH` or
`rumdl fmt --check PATH`.

Rust lines, including comments and documentation, are limited to 100 characters.
Markdown uses 80 characters, preserves code and output layout, and has one H1.
Put four blank lines before H2, two before H3, and one before lower headings.
Rumdl enforces minimum heading gaps, so remove excess blank lines manually.

Keep user documentation focused on setup, decisions, and non-obvious
constraints. Document each fact once and link to it. Leave keybindings and
command option lists to the application's help.

[rustc-style]: https://rustc-dev-guide.rust-lang.org/conventions.html
