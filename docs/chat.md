# Chat

Members join repository channels as IRC clients. Configure `[chat]` in the
[manifest](manifest.md#chat) to enable a separate Chatter role. It can answer
while Talker or Coder is busy. Without `[chat]`, that role does not start.




## Server requirements

The server must provide TLS, SASL authentication, and IRCv3 `account-tag`.
History replay additionally needs `message-tags`, `batch`, and stable `msgid`
values. Missing account tags prevent chat work. Missing history support prevents
backlog recovery.

The included [Ergo deployment](../deploy/ergo/README.md) provides these
features. Members need only outbound connections. People can use ordinary IRC
clients.




## Identity and channels

Each participant signs a manifest attestation binding their Radicle DID to an
IRC account, client certificate, repository, and pinned server. Every active
member needs an attestation. People directing members need one too, and their
DIDs must pass the manifest's trigger policy.

Authorization uses the SASL account asserted by the pinned server, never the
nick. The server is trusted to assert accounts honestly. Client certificates are
separate from Radicle signing keys. Authenticated messages receive
repository-wide tool scope only after the DID passes policy checks.

Use separate accounts and certificates for each repository. Bind channels to one
repository only and put its full RID in the topic. The topic catches
configuration mistakes, but is not an authorization control.




## Privacy

Require SASL, disable open registration, and restrict channels to their
repository's participants with invite-only, registered-only, and secret modes.

Messages are not end-to-end encrypted. The server operator can read them, and
stored history is plaintext. Set a retention limit.

The repository manifest exposes the server and channel names to repository
readers. Use administrator-owned local policy if those names must stay private.
