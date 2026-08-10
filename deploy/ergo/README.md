# Ergo deployment

Set `ERGO_SERVER_NAME` to the TLS hostname, then start the pinned Ergo release:

```console
ERGO_SERVER_NAME=irc.example.com docker compose -f deploy/ergo/compose.yaml up -d
docker compose -f deploy/ergo/compose.yaml logs ergo
```

The first start creates `/ircd/ircd.yaml`, a server certificate, and a random
operator password in the persistent volume. Replace the generated certificate
with a publicly trusted one when people use ordinary IRC clients. The member
transport pins its key and does not require a public CA.

The compose overrides make the server SASL-only, disable public account
registration, retain registered-channel history in SQLite, and expire it after
seven days. `localhost` is initially exempt so an operator can provision the
first accounts. Remove it after provisioning:

```console
ERGO_SASL_EXEMPTED='[]' docker compose -f deploy/ergo/compose.yaml up -d
```

Run `radicle-team setup` once for each repository member. It creates 
`<member folder>/irc-identity.pem` with mode `0600` and prints the certificate
fingerprint needed by the manifest.

Account and channel provisioning stays an operator action. From an exempt
connection, use the generated operator password and create a distinct SASL
account for every repository member with `NickServ SAREGISTER`; accounts and
certificates must not be shared across repositories. Attach the member
certificate with `NickServ CERT`, then register the channel. Set it invite-only,
registered-only, and secret, and grant persistent access only to that
repository's participants. Ergo documents the exact commands through
`NickServ HELP` and `ChanServ HELP`; this avoids encoding Ergo-specific services
syntax in `radicle-team`.

The server SPKI pin can be derived from its certificate with:

```console
openssl x509 -in fullchain.pem -pubkey -noout \
  | openssl pkey -pubin -outform DER \
  | openssl dgst -sha256
```

Prefix the lowercase hexadecimal digest with `spki-sha256:` in the manifest.
Back up the volume: account data, channel policy, and plaintext history live
there.
