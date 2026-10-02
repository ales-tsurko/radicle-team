# Setup and operation

Installing `rad-team`, provisioning a member, running it, and finding things
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
confine those by default, which makes `bwrap` fail with
`setting up uid map: Permission denied`. Confirm it works, and lift the
restriction if it does not:

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




## Install provider configs

Provider configurations are separate TOML files in the source distribution's
`providers/` directory. Install them before full member setup. The executable
reads these files and never installs or updates them.


### macOS

From the source checkout, run in Terminal:

```sh
provider_dir="${XDG_CONFIG_HOME:-$HOME/.config}/radicle-team/providers"
mkdir -p "$provider_dir"
cp providers/*.toml "$provider_dir/"
```

The default directory is `~/.config/radicle-team/providers`.


### Linux

From the source checkout, run in your shell:

```sh
provider_dir="${XDG_CONFIG_HOME:-$HOME/.config}/radicle-team/providers"
mkdir -p "$provider_dir"
cp providers/*.toml "$provider_dir/"
```

The directory is `$XDG_CONFIG_HOME/radicle-team/providers` when that variable is
set, otherwise `~/.config/radicle-team/providers`.

You can install only the files you need or add your own provider TOML files.
Choices come from the installed files. Missing files reduce the choices; an
empty directory produces an error with installation instructions. Invalid files
are reported and excluded from selection.

Edit installed configs in place. To update them, repeat the copy step with newer
files; this replaces local files with matching names. Updating the executable
does not update provider configs. Deleting a config removes that provider from
the available choices.




## Set up a member

Run setup once for each member and repository, in the directory where you keep
projects:

```sh
cd ~/Desktop
rad-team setup nico --repo rad:z3cyot...
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
├── state.db             deliveries, task queue, chat messages
├── sandbox.toml         what nico's agents may read, write, and reach
├── irc-identity.pem
├── node.log
├── cache/               build output, shared by nico's three roles
├── harness/             role configs and shared .providers/ sign-in state
└── code/
    ├── .git-common      commits shared by setup and all three roles
    ├── talker/          issues and patches
    ├── chatter/         chat
    └── coder/           writing code
```

`talker/`, `chatter/` and `coder/` each behave like a normal Git clone. They
share `.git-common`, so the history is stored once rather than three times, and
each has its own `HEAD` and index.

Before every delivery the runtime resets that role's copy to whatever it is
about to work on.

In the building copy, anything a previous run left uncommitted is committed to a
`nico-rt/salvage/...` branch first and logged, rather than thrown away. The
answering copies are simply cleaned: what they hold is what they looked at on
the way to an answer, and the answer itself is already in the issue or the
channel.

The building role uses one branch per task, named after that task:

```text
nico-rt/task-17
```

A task branch is removed once its task is over. The queue is what says so: a
task which was carried out has had its row removed, and a task which stalled,
failed, or was interrupted still has one, along with whatever it committed. A
branch the agent is still standing on is left alone and removed when it next
moves to other work, so tidying up never pulls the ground out from under a task
which is still running.

Setup also asks which models the member runs on, and signs you in if that
provider needs it. Answer "decide later" to leave it unset.

Pass `--home PATH` to name the folder yourself.


### Preparing access to a private repository

Create the identity before granting repository access:

```sh
rad-team setup nico --identity-only
```

This succeeds without starting a node, connecting to seeds, fetching the
repository, or configuring agents. It saves the identity in the member folder
and prints `nico identity: did:key:...`. Rerunning it prints the same DID.
`--repo` and `--home` are optional in this mode. Without a repository, setup
creates `./nico` in the current directory, and the identity is not yet bound to
a repository. `--home` (or `RADICLE_TEAM_HOME`) overrides the folder. Providing
`--repo` keeps the usual default folder naming.

As a repository delegate, grant that DID access using your own Radicle identity:

```sh
rad id update --repo rad:z3cyot... --title "Allow Nico" --allow did:key:...
```

The private repository also needs a reachable, authorized seed holding a copy.
Grant the trusted seed access and arrange replication as described in the
[Radicle private repository guide][private-repository-guide]. Then run full
setup with that seed's NID:

```sh
rad-team setup nico --home ./nico --repo rad:z3cyot... --seed z6Mk...
```

For full setup, point `--home` at the folder created in the first stage. Full
setup requires `--repo` and binds that folder to the selected repository while
reusing the authorized key. An already bound folder cannot be reassigned to
another member or repository.

The DID derives from the saved key. Restarts, application updates, and moving
the member folder preserve it. Deleting and recreating the identity generates a
new DID, requiring new access grants and a new manifest binding. Separate member
folders have separate identities by default; there is no universal DID for a
member name. Retrieve a saved DID without synchronizing:

```sh
RAD_HOME=./nico/identity rad self --did
```


### Choosing models

```sh
rad-team provider ls
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
settings, and how it accepts being authorized. Adding a model, a setting, or a
whole provider is an edited file rather than a new release.

```sh
rad-team provider login
```

Run these commands from the prepared member directory, or pass `--home PATH`.
`rad-team provider` prints the configured provider's name. `provider ls` marks
it with `*`. `provider login` reads that provider from the canonical manifest;
while initial setup approval is pending, it reads the member's setup proposal.
It never asks which provider to use.

Setup authorizes the selected provider for this member. An API key is saved in
its private `credentials/` directory and placed in each role's environment.
Subscription login opens the adapter's actual sign-in flow. Sign in once for all
of this member's roles; your personal adapter sign-in is separate. Native state
lives under `harness/.providers/<adapter>/` inside the member home. Claude Code
on macOS stores credentials in a keychain entry scoped to that directory. A
saved subscription choice is checked against the adapter's sign-in status.

To change the provider:

```sh
rad-team provider select
```

The menu labels the current provider with `(current)` and highlights it
initially. Choose and authorize the new provider, then review the proposed
manifest. The command asks whether to publish a Radicle patch. Declining leaves
the manifest change in `code/coder/radicle-team.toml` for manual publication.
The current provider remains active until the manifest change reaches the
canonical branch. After merging, restart `rad-team run` to apply it. Selection
requires the member's initial setup patch to have merged first.

Provider configs are [installed separately](#install-provider-configs). Changing
model settings in a provider's TOML file takes effect on the next start;
rebuilding the executable is unnecessary.


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
When the canonical manifest already authorizes the member, setup reports that
the member is ready. Otherwise, interactive setup publishes a manifest patch and
finishes with:

```text
Setup complete; awaiting manifest approval.
```

When standard input or output is not a terminal, setup never prompts or
publishes a patch. It prints the required action and exits.


### Adding the member to the manifest

Setup works from any directory, including on a separate server. It initializes
the member's three role worktrees and uses `code/coder/` to propose the
manifest. The worktrees share `code/.git-common`. Setup keeps the coder worktree
on a detached `HEAD`, commits after confirmation, and publishes a patch under
the member's signing identity. The operator's checkout and canonical branch are
untouched.

The wizard saves the proposed `radicle-team.toml` in the coder worktree and asks
before committing and publishing. Rerunning an interrupted setup resumes from
the working manifest or detached commit and reuses an existing Radicle patch.
The member lock prevents the runtime from using the worktree during setup. Once
published, the patch preserves the commit when the coder moves to task branches.
Setup refuses to replace unrelated uncommitted work.

Setup keeps the member's node running until at least one repository seed
confirms fetching the published patch. If replication fails or times out, setup
reports the error and retains the patch so rerunning setup can retry its
replication.

Setup finishes once the patch is replicated and local configuration is ready. A
repository delegate reviews and merges the patch into the canonical branch. You
can start `run` before the merge: it waits for the approved manifest before
starting agents or processing repository work. A proposed manifest never
authorizes the member. An invalid binding or a binding for another DID is
reported as an error.

With `--manifest PATH`, setup updates the explicitly selected local manifest
instead, then validates it. This mode uses administrator-owned local policy and
does not require a canonical merge.


### Creating a manifest from scratch

The wizard asks who may trigger the repository's artificial contributors, then
whether to create the security-report repository or use an existing RID. When
creating one, it asks for a recognizable name, such as `my-project-security`.
Names follow Radicle's rules: letters, numbers, `-`, `_`, and `.`. It grants the
member DID access, generates an age encryption identity, and shows its recovery
key once — save that key outside every agent environment. Later members are
granted access through the same repository authority.

Each newly created security repository has a distinct RID, including when a name
is reused, so another team or a repeated setup attempt does not collide with an
earlier creation.

That repository must be private, the user's own Radicle node must be running,
and the user's Radicle identity must be able to update its access list. A public
repository is refused.

Setup unlocks your personal Radicle identity before creating the repository or
granting access. Your identity owns newly created repositories; the member DID
receives access. The repository stays in your Radicle storage after its
temporary initial checkout is removed. Use `rad checkout <RID>` to create a
working copy.




## Run a member

After setup finishes, start the watcher from the member directory or any of its
subdirectories:

```sh
rad-team run
```

From elsewhere, select the member directory explicitly:

```sh
rad-team run --home ./nico-z3cyot...
```

`run` reads both the member and repository ID recorded by setup. It accepts
neither a member argument nor `--repo`. An ordinary repository checkout does not
contain the member's identity and state; run setup first.

The process owns the member's node for its lifetime. If manifest approval is
pending, it keeps synchronizing and waits for the canonical binding. Once
approved, it classifies relevant assignments, mentions, replies, comments,
reviews, revisions, reactions, resolutions, lifecycle changes, and merges until
stopped. `SIGINT` and `SIGTERM` trigger graceful node shutdown before the child
is reaped.

With fresh state, `run` fixes the conversation cutoff when it starts the
member's node. It skips replies and mentions created before that cutoff,
including older history fetched afterward. Replies and mentions created from
startup onward are handled, even if synchronization or initialization is still
in progress. Current open assignments enter the queue regardless of age.
Restarting preserves the cutoff and checkpoints and retries unfinished
deliveries. Task discovery continues while the talking agent is busy. Radicle
timestamps have second precision, so activity in the startup second counts as
new.

Operational events are emitted as structured records through `tracing`, so the
destination and verbosity are chosen by the process rather than fixed in the
library. Logging defaults to `warn`. Use `--log` to select the verbosity:

```sh
rad-team run --log info
rad-team run --log debug
```

The flag also accepts `error`, `warn`, `trace`, and `off`, and works before or
after any subcommand. Without `--log`, `RUST_LOG` still accepts levels and
filters such as `radicle_team=debug`. An explicit `--log` overrides `RUST_LOG`.

At `info` the process reports what it decided and why:

- identity unlocking, node startup, seed connections, and synchronization
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
rad-team run --home ~/Desktop/nico-z3cyot...
rad-team run --home ~/Desktop/maya-z3cyot...
```

Each gets its own folder, side by side:

```text
~/Desktop/nico-z3cyot.../
~/Desktop/maya-z3cyot.../
```

Run `rad-team run` from inside one of them, or from anywhere with `--home`.
Commands search the current directory and its parents for
`radicle-team-member.toml`, the way `git` finds a repository. A file of that
name which does not read back as a marker does not end the search, since
everything below a member's folder is repository content.

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
- `setup --identity-only` saves the identity and prints its DID before
  repository access is granted; `--repo` and `--home` are optional. Without
  either, it creates a folder named after the member in the current directory.
  It does not fetch or run the remaining setup steps
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
paths relatively and new stored passphrases are keyed on the member and DID, so
renaming or relocating the folder keeps working. Existing passphrases keyed on
the member and repository remain supported. This needs Git 2.48 or newer, and
setup refuses an older one rather than failing obscurely later.

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

For a private repository, run `setup --identity-only` first to obtain the DID,
grant it access, and provide a reachable authorized seed with `--seed`. If a
previous full setup attempt failed, identity-only setup reuses its saved key.
Node startup failures include the log path in their diagnostic.

[private-repository-guide]: https://radicle.dev/guides/user#curating-the-allow-list
