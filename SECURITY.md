# Security policy

Report vulnerabilities privately to
[ales.tsurko@gmail.com](mailto:ales.tsurko@gmail.com), with the affected version
and a minimal reproduction using dummy credentials. Keep exploit details and
real secrets out of public issues, pull requests, and chat.

Security fixes target the latest `main` branch during pre-release.




## Local checks

```sh
gitleaks git --redact=100 --ignore-gitleaks-allow --log-opts="--all"
make audit
```

The audit excludes [RUSTSEC-2023-0071]: Radicle's SSH dependency includes an
unpatched RSA vulnerability, but this application uses Ed25519 identities and
exposes no RSA private-key operations.

Zerobox still brings in `hickory-proto` 0.25.2. [RUSTSEC-2026-0118] requires
DNSSEC, which this build disables. [RUSTSEC-2026-0119] concerns encoding many
records, while this resolver encodes single-question queries and decodes
replies. Source inspection found no reachable attack path in this configuration.
Both remain reported. Reassess these findings if signing or DNS usage changes.

[RUSTSEC-2023-0071]: https://rustsec.org/advisories/RUSTSEC-2023-0071.html
[RUSTSEC-2026-0118]: https://rustsec.org/advisories/RUSTSEC-2026-0118.html
[RUSTSEC-2026-0119]: https://rustsec.org/advisories/RUSTSEC-2026-0119.html
