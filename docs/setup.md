# Setup and operation

Installing `radicle-team`, provisioning a member, running it, and finding things
when something goes wrong. Configuration lives in
[the manifest reference](manifest.md) and [the sandbox reference](sandbox.md).




## Prerequisites

Install Radicle so that `rad` and `radicle-node` are available on `PATH`. You
also need the repository RID and permission to propose or approve its project
manifest.

On Linux, install `bubblewrap` as well. Zerobox launches every sandboxed agent
process through it, so without `bwrap` on `PATH` the sandbox cannot start and
each agent command fails. macOS needs nothing extra, because it uses the
operating system's own sandbox.

```sh
sudo apt-get install bubblewrap   # or the equivalent for your distribution
```

Bubblewrap also needs an unprivileged user namespace. Ubuntu 23.10 and later
confine those by default, which makes `bwrap` fail with `setting up uid map:
Permission denied`. Confirm it works, and lift the restriction if it does not:

```sh
bwrap --dev-bind / / --unshare-user true
sudo sysctl --write kernel.apparmor_restrict_unprivileged_userns=0
```

Add `kernel.apparmor_restrict_unprivileged_userns=0` to `/etc/sysctl.d/` to make
that persist across reboots.

Build the application from this repository:

```sh
cargo install --path .
```




## Set up a member

Run setup once for each member and repository, in the directory where you keep
projects:

```sh
cd ~/Desktop
radicle-team setup nico --repo rad:z3cyot...
```

This creates a folder named after the member and the repository, and everything
Nico needs goes inside it:

```text
~/Desktop/nico-z3cyot.../
├── .radicle-team        marks the folder and records member, repo, layout
├── identity/            nico's key, storage, and node
├── state.db             deliveries, job queue, chat messages
├── sandbox.toml         what nico's agents may read, write, and reach
├── irc-identity.pem
├── node.log
├── cache/               build output, one folder per adapter
├── harness/             agent state, one folder per adapter
└── code/
    ├── .git-common      the commits, stored once
    ├── talker/          issues and patches
    ├── chatter/         chat
    └── coder/           writing code
```

`talker/`, `chatter/` and `coder/` each behave like a normal Git clone. They
share `.git-common`, so the history is stored once rather than three times, and
each keeps its own branch.

Before every delivery the runtime resets that adapter's copy to whatever it is
about to work on.

In the building copy, anything a previous run left uncommitted is committed to a
`radicle-team/salvage/...` branch first and logged, rather than thrown away. The
answering copies are simply cleaned: what they hold is what they looked at on
the way to an answer, and the answer itself is already in the issue or the
channel.

The building adapter commits to `radicle-team/patch/<id>`, and that branch is
removed once the patch merges. A job taken from an issue starts on
`radicle-team/issue/<id>`, since the patch it will produce does not exist yet,
and moves onto that patch's branch the moment it publishes one. The queue is
told at the same time, so a job which crashes afterwards comes back as work on
that patch rather than starting the issue over, and nothing it commits in
between is stranded under a name the queue no longer knows. A job which
publishes a second patch stays with the first, so the branch and the queue never
name different ones. A job which published nothing keeps its issue name, and
that branch goes when the issue is settled.

A branch the adapter is still standing on is left alone and removed when it next
moves to other work, so a landed patch never pulls the ground out from under a
job which is still running. Every start reads the issues and patches back and
removes the branches of anything settled while nobody was running.

Pass `--home PATH` to name the folder yourself.


### Looking inside a member's copies

Read them freely. `git status`, `git log`, `git diff` and `git show` work as
they do in any clone, and they are the quickest way to see what a member is
holding right now.

`rad` is the exception. The copies carry a `rad` remote, but `rad` finds its
identity through `RAD_HOME`, which in a shell points at your own profile rather
than the member's. Point it at the member's to ask about the member:

```sh
RAD_HOME=~/Desktop/nico-z3cyot.../identity rad patch list
```

**Do not edit anything in there, and do not commit to it.** Before every
delivery the runtime resets that copy hard and cleans it, so an uncommitted edit
is gone the next time the member is addressed — silently, in the answering
copies. The `radicle-team/...` branches belong to the runtime, which moves and
deletes them on its own schedule. And an edit made while the member is working
races with its Git, in a directory that member is about to overwrite.

Work in your own clone instead, and let the member reach it through Radicle like
anybody else:

```sh
rad clone rad:z3cyot...
```

Every command afterwards checks that the folder belongs to the member and
repository it was asked to run, and refuses if it does not, so a member cannot
publish under another's identity by being started in the wrong directory.

Setup creates or loads Nico's identity, starts Nico's node, synchronizes the
repository, records a seeding policy, and prints the DID. That DID is Nico's
own, not the operator's, so a private repository has to grant it access before
synchronization can succeed — the wizard waits and retries while that happens.
It finishes with:

```text
nico is ready for rad:... with binding generation 1
```

When standard input or output is not a terminal, setup never prompts or modifies
the checkout. It prints the required action and exits.


### Adding the member to the manifest

If the repository manifest or member binding is missing, run setup from the
repository checkout. It shows the exact Git commands and asks before writing
`.radicle-team.toml`, committing, and opening a Radicle patch under the user's
identity.

It publishes only when `HEAD` exactly matches the live canonical `rad` tip, so
switch to an up-to-date canonical branch first and unrelated commits can never
be swept in. Once the patch reaches the canonical branch, the wizard validates
it.


### Creating a manifest from scratch

The wizard asks who may trigger the repository's artificial contributors, then
whether to create the security-report repository or use an existing RID. It
grants the member DID access, generates an age encryption identity, and shows
its recovery key once — save that key outside every agent environment. Later
members are granted access through the same repository authority.

That repository must be private, the user's own Radicle node must be running,
and the user's Radicle identity must be able to update its access list. A
public repository is refused.




## Run a member

After setup reports that the member is ready, start the watcher:

```sh
radicle-team run nico --repo rad:...
```

The process owns the member's node for its lifetime. It classifies relevant
assignments, mentions, replies, comments, reviews, revisions, reactions,
resolutions, lifecycle changes, and merges until stopped. `SIGINT` and
`SIGTERM` trigger graceful node shutdown before the child is reaped.

Operational events are emitted as structured records through `tracing`, so the
destination and verbosity are chosen by the process rather than fixed in the
library. `RUST_LOG` selects both, defaulting to `info`:

```sh
RUST_LOG=radicle_team=debug radicle-team run nico --repo rad:...
```

At `info` the process reports what it decided and why:

- activity dispatched to an adapter, or ignored and for which reason
- a mutation published, or recovered from an interrupted delivery
- an MCP method the manifest no longer allows
- the sandbox becoming ready

`debug` adds each repository scan, each classified activity, and the policy
built for every sandboxed command.

The ignored-activity records matter most in practice. Without them a
`triggered_by` decision, or a conversation that has run its turns, looks exactly
like a member that is simply not responding.




## Run multiple members

Set up and run each member independently:

```sh
radicle-team run nico --repo rad:...
radicle-team run maya --repo rad:...
```

Each gets its own folder, side by side:

```text
~/Desktop/nico-z3cyot.../
~/Desktop/maya-z3cyot.../
```

Run `radicle-team run` from inside one of them, or from anywhere with `--home`.
Commands search the current directory and its parents for `.radicle-team`, the
way `git` finds a repository.

Two processes cannot use one member folder at the same time. The same member can
run for different repositories, in different folders, without sharing identity
or state.




## Command-line options

Invocation-specific options stay command-line arguments rather than manifest
values:

- `--home PATH` names the member folder, overriding `RADICLE_TEAM_HOME` and the
  upward search
- `--seed NID` adds a preferred synchronization seed and may be repeated, on
  `setup` and on `run`, which synchronizes before it starts watching
- `--identity-home PATH` uses an existing dedicated Radicle identity
- `--expected-did DID` verifies that existing identity
- `--unencrypted-identity` creates a managed key protected only by filesystem
  permissions

An encrypted `--identity-home` must provide its passphrase through
`RAD_PASSPHRASE` or the matching operating-system credential. SSH-agent access
alone cannot unlock the managed `radicle-node` process.




## Local state

Everything is inside the member folder, so it can be moved, backed up, or
deleted as a unit. There is no global directory. Working copies record their
paths relatively and the stored passphrase is keyed on the member and
repository, so renaming or relocating the folder keeps working. This needs Git
2.48 or newer, and setup refuses an older one rather than failing obscurely
later.

Managed private keys live in `identity/`. Encrypted-key passphrases live in the
operating-system credential store, never in the manifest. Node output goes to
`node.log`.

`cache/` holds build output and downloaded dependencies. It is disposable, and
`rm -rf cache/` is always safe. Nothing that has to survive belongs there.

`harness/` holds whatever each agent tool writes for itself, kept inside the
member folder so deleting the folder deletes it too. Nothing is resumed between
deliveries: every run starts fresh and reads the issue or patch it is answering
through MCP, which is the record everyone can see.

Managed Radicle commands ignore an inherited `RAD_SOCKET`. Each member always
uses the control socket inside its own `identity/`.




## Troubleshooting

A member folder written by an older version of `radicle-team` is refused by
name rather than ignored, so a stale layout cannot quietly produce a second
identity. Set those members up again in their own folders.

If setup fails because a repository is private, use the DID shown in the error
or identity log message to grant access, then rerun setup. Node startup failures
include the log path in their diagnostic.
