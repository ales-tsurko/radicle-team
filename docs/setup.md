# Setup and operation




## Prerequisites

- Linux or macOS, Rust, and Git 2.48 or newer.
- Radicle's `rad` and `radicle-node` on `PATH`.
- The repository RID and access to its delegates for manifest approval.
- An installed [agent application](adapters/index.md) for your provider.
- On Linux, `bubblewrap` and working unprivileged user namespaces.

Check Linux sandbox support with:

```sh
bwrap --dev-bind / / --unshare-user true
```

On Ubuntu, `setting up uid map: Permission denied` can mean AppArmor restricts
unprivileged user namespaces. An administrator can change that policy with
`kernel.apparmor_restrict_unprivileged_userns=0`. This changes host policy, so
apply it only where appropriate.




## Install

From the source checkout:

```sh
cargo install --path . --locked
provider_dir="${XDG_CONFIG_HOME:-$HOME/.config}/radicle-team/providers"
mkdir -p "$provider_dir"
cp providers/*.toml "$provider_dir/"
```

The provider directory is the same on Linux and macOS. Copy only the files you
need, or customize the installed copies. Executable updates do not replace
provider configs. Copying newer files over them replaces local edits.




## Set up a member

Keep member homes outside the source checkout:

```sh
rad-team setup nico --repo rad:... --home ../nico
```

Setup creates the member's identity, synchronizes the repository, configures its
provider and sandbox, and offers to publish a manifest patch. A repository
delegate must merge that patch into the canonical branch. You can start the
member beforehand, but it waits for approval before running agents.

When creating a manifest, setup also configures a private repository for
security reports. Creating it or changing its access list requires your own
running Radicle node and identity. Save the age recovery key outside all agent
environments when setup shows it.

`--manifest PATH` instead updates administrator-owned local policy directly,
without a repository patch. Noninteractive setup does not prompt or publish.


### Private repositories

Create the identity before granting access:

```sh
rad-team setup nico --identity-only --home ../nico
```

As a delegate, allow the printed DID and arrange an authorized seed holding the
repository, following the [Radicle private repository guide][private-repos].
Then finish setup with the same member home:

```sh
rad id update --repo rad:... --title "Allow Nico" --allow did:key:...
rad-team setup nico --home ../nico --repo rad:... --seed z6Mk...
```

The DID belongs to this saved identity. Recreating the identity requires new
access grants and a new manifest binding. To retrieve its DID later:

```sh
RAD_HOME=../nico/identity rad self --did
```




## Run

```sh
rad-team run --home ../nico
```

Use one process and one home per member and repository. Inside a member home,
commands also discover it from the current directory or its parents.

Interactive terminals show a dashboard. For services or redirected output, use
plain logs. To retain logs beyond the dashboard's history:

```sh
rad-team run --home ../nico --plain --log info --log-file nico.log
```

Logs default to `warn`. Use `--log info` for delivery decisions or `debug` for
agent events. `--log` overrides `RUST_LOG`. Stopping the runtime also stops its
managed Radicle node.

Assign issues for implementation and patches for review. Address members with
`@nico` or `@team`, subject to the [manifest's trigger policy](manifest.md). An
acknowledgment reaction means queued, not completed. Review tasks finish only
after publishing the requested review. Implementation finishes with a published
patch ready for review.




## Providers

From the member home:

```sh
rad-team provider ls
rad-team provider login
rad-team provider select
```

Setup and `provider select` authorize a provider for all of this member's roles.
Its sign-in is separate from your personal agent application's sign-in.
`provider login` reauthorizes the currently configured provider.

Changing providers proposes a manifest patch. The current provider stays active
until that patch merges, then a restart applies the change. Declining
publication leaves the proposal in `code/coder/radicle-team.toml`.

See [providers and adapters](adapters/index.md) for configuration and network
requirements.




## Inspect work

From the member home, including while it is stopped:

```sh
rad-team status
rad-team task ls
rad-team task show 17
rad-team inbox ls
rad-team inbox show <review-id>
```

Tasks contain unfinished work. Inbox entries explain whether a notification was
queued, handled, ignored, or failed. These commands read state without starting
a node or unlocking signing keys.

To inspect signed delivery receipts from another machine, synchronize your own
Radicle storage first:

```sh
rad sync --fetch rad:...
rad-team inbox show <review-id> --repo rad:... --member maya
```

"Not observed locally" does not establish whether the remote member received it.
If a review has not reached the member, announce it from the reviewer's Radicle
profile:

```sh
rad sync --announce --replicas 1 rad:...
```




## Local state

A member home contains identity and node storage in `identity/`, runtime state
in `state.db`, policy in `sandbox.toml`, agent data in `harness/`, worktrees in
`code/`, and disposable build output in `cache/`. Node diagnostics are in
`node.log`. Key passphrases live in the operating-system credential store.

Back up the home to preserve the member's identity and state. Provider configs
are installed separately. Existing encrypted identities need their passphrase
available to the managed node through `RAD_PASSPHRASE` or the operating-system
credential store. SSH-agent access alone is insufficient.

Read the worktrees for inspection, but make your own changes in a separate
clone. The runtime resets its worktrees between deliveries and owns their
branches. When using `rad` to inspect the member's storage, set `RAD_HOME` to
its `identity/` directory rather than using your own profile.

For access failures, check the private repository's grants and seed. For an
ignored notification, inspect its inbox reason and
[trigger policy](manifest.md#who-may-put-a-member-to-work). For process
failures, check `node.log` and the [adapter requirements](adapters/index.md).

[private-repos]: https://radicle.dev/guides/user#curating-the-allow-list
