# Security policy

Security fixes target the latest `main` branch while this project is
pre-release.




## Reporting a vulnerability

Email [ales.tsurko@gmail.com](mailto:ales.tsurko@gmail.com) privately. Include
the affected commit or version, operating system, expected security boundary,
and a minimal reproduction using disposable identities and dummy credentials.

Do not disclose vulnerability details in public issues, pull requests, or chat.
Do not include real API keys, private keys, provider sign-in state, or
unredacted member homes, databases, logs, or transcripts. Coordinate any
additional sensitive material privately before sending it.

Please allow time to investigate and coordinate a fix before public disclosure.




## Dependency advisories

Run `make audit` to check the lockfile against RustSec. Vulnerabilities and
soundness warnings fail the check. Maintenance warnings remain visible.

The check has one explicit exception: [RUSTSEC-2023-0071], an unpatched RSA
timing vulnerability pulled in by Radicle's SSH agent dependency. Radicle Team
uses Ed25519 signing identities. Radicle's keystore rejects non-Ed25519 keys,
and its agent signer accepts only an Ed25519 verification key. This application
does not expose RSA private-key operations. Reassess this exception before
adding another signing algorithm or using RSA through the SSH dependency.

Zerobox's registry dependencies still pull in `hickory-proto` 0.25.2, which
triggers [RUSTSEC-2026-0118] and [RUSTSEC-2026-0119]. They are not suppressed.
The DNSSEC vulnerability requires features that this project's build does not
enable. The encoding vulnerability requires messages with many records. The
resolver used by the network proxy encodes single-question queries and decodes
upstream replies. Source and feature inspection found no reachable attack path
for either advisory in this configuration. Reassess when changing resolver 
features or DNS usage.

Upstream maintenance warnings for `basic-toml`, `zbus_names`, `zvariant`, and
`zvariant_derive` remain. These come through age's localization tooling and the
Linux Secret Service backend. They are not suppressed by the audit.

[RUSTSEC-2023-0071]: https://rustsec.org/advisories/RUSTSEC-2023-0071.html
[RUSTSEC-2026-0118]: https://rustsec.org/advisories/RUSTSEC-2026-0118.html
[RUSTSEC-2026-0119]: https://rustsec.org/advisories/RUSTSEC-2026-0119.html
