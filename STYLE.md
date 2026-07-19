# Style guide

Follow the
[Rust Style Guide](https://doc.rust-lang.org/style-guide/) and the
[rustc coding conventions][rustc-style].
Nightly `rustfmt` is the Rust formatting authority.




## Module layout

Write modules from the public concept down to its details:

1. Module documentation.
2. `std`/`core` imports, third-party imports, then current-crate imports.
   Current-crate paths must start with `crate::`.
3. Re-exports, module declarations, then constants.
4. The module's primary type, immediately followed by all of its inherent and
   trait impls.
5. Supporting types in the order the primary abstraction introduces them,
   each immediately followed by its impls.
6. Private utilities, error types, and tests, in that order.

Keep an item earlier only when Rust's lexical rules require it, such as a macro
used by the primary implementation.




## Functions and types

- Prefer methods on the type that owns the operation's state or context. Avoid
  standalone functions unless the operation has no natural owner; `main`,
  tests, and trait-required functions are normal exceptions.
- Use traits and conversion types such as `From` when they make an extension
  point explicit and keep variant-specific logic localized.
- Derive errors with `thiserror`; do not write manual `std::error::Error`
  implementations.




## Formatting


### Rust

- Limit Rust lines to 100 characters, including documentation and comments.
- Use four-space indentation and standard Rust naming, whitespace, and item
  formatting.
- Run `make fmt` and `make lint` before submitting changes.


### Markdown

- Limit Markdown lines to 80 characters.
- Use exactly one H1 heading as the document title.
- Put four blank lines before each H2 heading.
- Put two blank lines before each H3 heading.
- Put one blank line before each lower-level heading.

[rustc-style]: https://rustc-dev-guide.rust-lang.org/conventions.html
