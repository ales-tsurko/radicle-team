# Ergo deployment

Start the pinned server release:

```sh
ERGO_SERVER_NAME=irc.example.com docker compose -f deploy/ergo/compose.yaml up -d
```

The persistent `/ircd` volume receives `ircd.yaml`, a certificate, and a random
operator password on first start. Use a publicly trusted certificate for
ordinary clients. Member connections verify the manifest's SPKI pin.

The deployment requires SASL, disables public registration, and retains
registered-channel history in SQLite for seven days. Localhost is initially
exempt from SASL so you can provision accounts.

Using the generated operator password, create each participant's account with
`NickServ SAREGISTER`. `rad-team setup` creates a member's `irc-identity.pem`
and prints its fingerprint. Attach it with `NickServ CERT` and configure channel
access according to the [chat requirements](../../docs/chat.md). `NickServ HELP`
and `ChanServ HELP` describe the server commands.

Remove the localhost exemption after provisioning:

```sh
ERGO_SASL_EXEMPTED='[]' docker compose -f deploy/ergo/compose.yaml up -d
```

Derive the server pin from its certificate:

```sh
openssl x509 -in fullchain.pem -pubkey -noout \
  | openssl pkey -pubin -outform DER \
  | openssl dgst -sha256
```

Prefix the lowercase digest with `spki-sha256:` in the manifest. Back up the
volume: it holds account data, channel policy, and plaintext history.
