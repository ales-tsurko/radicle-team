# Setup and operation

Installing `radicle-team`, provisioning a member, running it, and finding things
when something goes wrong. Configuration lives in
[the manifest reference](manifest.md) and [the sandbox reference](sandbox.md).




## Prerequisites

Linux or macOS. Windows is not supported and the crate refuses to compile there:
every agent runs inside a sandbox and is given unrestricted permissions within
it, so a build without one would run agents unconfined.

Install Radicle so that `rad` and `radicle-node` are available on `PATH`. You
also need the repository RID and permission to propose or approve its project
manifest.

Install the agent application the provider you pick is reached through, on the
same `PATH`. Which that is, and what it needs, is in
[adapters and providers](adapters/index.md).

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
cargo install --path . --locked
```




## Set up a member

Run setup once for each member and repository, in the directory where you keep
projects:

```sh
cd ~/Desktop
radicle-team setup nico --repo rad:z3cyot...
```

Setup starts and stops its own Radicle node; no running node is required. If
macOS asks for keychain access, approve it to unlock the member identity. The
spinner shows each stage, including seed connections and synchronization.

This creates a folder named after the member and the repository, and everything
Nico needs goes inside it:

```text
~/Desktop/nico-z3cyot.../
├── radicle-team-member.toml   marks the folder, records which member owns it
├── identity/            nico's key, storage, and node
├── state.db             deliveries, job queue, chat messages
├── sandbox.toml         what nico's agents may read, write, and reach
├── irc-identity.pem
├── node.log
├── cache/               build output, shared by nico's three roles
├── harness/             agent state, one folder per role
└── code/
    ├── .git-common      the commits, stored once
    ├── talker/          issues and patches
    ├── chatter/         chat
    └── coder/           writing code
```

`talker/`, `chatter/` and `coder/` each behave like a normal Git clone. They
share `.git-common`, so the history is stored once rather than three times, and
each keeps its own branch.

Before every delivery the runtime resets that role's copy to whatever it is
about to work on.

In the building copy, anything a previous run left uncommitted is committed to a
`nico-rt/salvage/...` branch first and logged, rather than thrown away. The
answering copies are simply cleaned: what they hold is what they looked at on
the way to an answer, and the answer itself is already in the issue or the
channel.

Only the building role has branches. Each one belongs to one job and is named
after it:

```text
nico-rt/job-17
```

A branch is removed once its job is over. The queue is what says so: a job which
was carried out has had its row removed, and a job which stalled, failed, or was
interrupted still has one, along with whatever it committed. A branch the agent
is still standing on is left alone and removed when it next moves to other work,
so tidying up never pulls the ground out from under a job which is still
running.

Setup also asks which models the member runs on, and signs you in if that
provider needs it. Answer "decide later" to leave it unset.

Pass `--home PATH` to name the folder yourself.


### Choosing models

```sh
radicle-team providers
```

```text
~/.config/radicle-team/providers

  anthropic    claude-code  not authorized
  deepseek     opencode     not authorized
  glm          opencode     not authorized
  gpt          opencode     not authorized
  grok         opencode     not authorized
  kimi         opencode     not authorized
  ollama       opencode     none needed
  qwen         opencode     not authorized
```

The same place on macOS as on Linux, and `XDG_CONFIG_HOME` moves it. The listing
prints the directory, so there is no need to guess.

One file per provider, named after it. The file says which agent application
reaches that provider's models, how a member's scores become that provider's
settings, and how it accepts being authorized. Adding a model, a
setting, or a whole provider is an edited file rather than a new release.

```sh
radicle-team login gpt
```

Once per provider, per machine — one person's account is one account however
many members use it. Add `--home PATH` to authorize one member alone, which is
what you want when a project has to be billed to its own account. Pass the same
`--home PATH` to `providers` to see that login; without it the listing reports
only the machine-wide one.

The key you give is placed in the agent's environment under the variable that
provider names, and nowhere else.

A provider you are already signed in to through its agent application is
recorded as a subscription instead of asking for a key, which is how a
subscription rather than an API key pays for a member. The sign-in itself lives
with that application, under the member's own `harness/<role>/` folder.

We replace the files we ship when the application is updated, so keep your own
edits under a different name. Credentials are never touched. A provider file we
have stopped shipping is left where it is, so an older one keeps working until
you remove it.


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
copies. The `nico-rt/...` branches belong to the runtime, which moves and
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

Setup creates or loads Nico's identity, starts Nico's node, waits for seed
connections, synchronizes the repository, and prints the DID. That DID is Nico's
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
`radicle-team.toml`, committing, and opening a Radicle patch under the user's
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
library. `RUST_LOG` selects the verbosity, defaulting to `warn`:

```sh
RUST_LOG=radicle_team=debug radicle-team run nico --repo rad:...
```

At `info` the process reports what it decided and why:

- activity dispatched to a role, or ignored and for which reason
- a mutation published, or recovered from an interrupted delivery
- an MCP method the manifest no longer allows
- the sandbox becoming ready

`debug` adds each repository scan, each classified activity, and the policy
built for every sandboxed command.

The ignored-activity records matter most in practice. Without them a
`triggered_by` decision, or a conversation that has run its turns, looks exactly
like a member that is simply not responding.


### What a delivery actually runs

Starting a member starts its three roles, and each one checks itself before any
work arrives: the agent application has to be there, and it has to offer the
model the provider names for this member's INT. A missing application, a
mistyped model, and a provider nobody logged into all stop the member here
rather than on its first delivery.

Each delivery is then one run of that application in the role's working copy,
given the member's documents, the run-scoped MCP server, and a short message
saying what happened, who did it, and where. Everything else it reads for itself
through MCP, which is the record everybody else reads too. Nothing is resumed
between deliveries.

How one particular agent application is driven, what it writes, and what it
needs beyond the provider's API is in
[adapters and providers](adapters/index.md).




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
Commands search the current directory and its parents for
`radicle-team-member.toml`, the way `git` finds a repository. A file of that name
which does not read back as a marker does not end the search, since everything
below a member's folder is repository content.

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

One folder per role, holding that role's `instructions/` — the member's
documents, written out of this build every time it starts — and whatever its
agent application keeps for itself. Removing it costs whatever that application
downloads again on the next delivery, and nothing else.

Managed Radicle commands ignore an inherited `RAD_SOCKET`. Each member always
uses the control socket inside its own `identity/`.




## Troubleshooting

If setup fails because a repository is private, use the DID shown in the error
or identity log message to grant access, then rerun setup. Node startup failures
include the log path in their diagnostic.
