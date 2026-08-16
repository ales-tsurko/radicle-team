//! Listing the model providers a machine has, and authorizing one.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use crate::provider::{self, Credentials, Way, credentials};

/// Lists the providers found under this machine's configuration directory.
///
/// # Errors
///
/// Returns an error when the configuration directory cannot be resolved, written, or read.
pub fn list_configured(home: Option<PathBuf>) -> Result<(), Error> {
    list(&provider::configuration()?, home.as_deref())
}

/// Authorizes `name` under this machine's configuration directory.
///
/// # Errors
///
/// Returns an error when the configuration directory cannot be resolved, or authorizing fails.
pub fn login_configured(name: &str, home: Option<PathBuf>) -> Result<(), Error> {
    login(&provider::configuration()?, name, home)
}

/// Lists every provider this machine can run a member on.
///
/// Installs the providers this build ships with first, so the list is never empty on a machine
/// which has not been set up and so a stale model name is corrected by updating the application.
///
/// # Errors
///
/// Returns an error when the provider directory cannot be written or read.
pub fn list(configuration: &Path, home: Option<&Path>) -> Result<(), Error> {
    let providers = provider::installed(configuration)?;
    let held = credentials::directory(configuration);
    // The same precedence `authorize` applies. A listing which looked only at the machine-wide
    // store would report a `login --home` as never having happened, and the operator would log in
    // again without the flag -- putting the project's key in the shared store.
    let mine = home.map(credentials::directory);

    println!("{}\n", provider::directory(configuration).display());
    for (name, provider) in &providers {
        let (adapter, state) = match provider {
            // One unreadable file must not take the table with it, in either direction. The
            // operator needs to see which providers are fine to work out what to do about the one
            // that is not.
            Err(error) => ("-".to_owned(), format!("unusable: {error}")),
            Ok(provider) => (
                provider.adapter().to_owned(),
                match credentials::load(&held, mine.as_deref(), name) {
                    Ok(credentials) => status(provider, credentials.as_ref()).to_owned(),
                    Err(error) => format!("unreadable: {error}"),
                },
            ),
        };
        println!("  {name:<12} {adapter:<12} {state}");
    }

    Ok(())
}

// Says whether a provider can be used yet, in the words the operator has to act on.
fn status(provider: &provider::Provider, credentials: Option<&Credentials>) -> &'static str {
    match credentials {
        Some(credentials) => credentials.way().id(),
        None if !provider.needs_authorizing() => "none needed",
        None => "not authorized",
    }
}

/// Authorizes `name` and records what it takes to reach it.
///
/// Writes into the member's own directory when one is given, which is what separates a project
/// billed to its own account from everything else on the machine.
///
/// # Errors
///
/// Returns an error when the provider is unknown, the prompt fails, or the credentials cannot be
/// written.
pub fn login(configuration: &Path, name: &str, home: Option<PathBuf>) -> Result<(), Error> {
    let provider = provider::installed_named(configuration, name)?;
    let path = login_with(&provider, configuration, name, home)?;
    cliclack::outro(format!("{name} authorized in {}", path.display()))?;

    Ok(())
}

// Authorizes a provider the caller is already holding, so the setup wizard does not re-read and
// re-parse the whole directory to reach the one it just offered.
//
// Returns where it was written rather than announcing it. `outro` closes an interactive session,
// which is right for `login` and wrong in the middle of a wizard that has more to ask.
pub(super) fn login_with(
    provider: &provider::Provider,
    configuration: &Path,
    name: &str,
    home: Option<PathBuf>,
) -> Result<PathBuf, Error> {
    let credentials = supply(provider, name)?;

    credentials::store(&destination(configuration, home), name, &credentials).map_err(Error::from)
}

// Returns where a login is recorded: this member alone, or everything on the machine.
fn destination(configuration: &Path, home: Option<PathBuf>) -> PathBuf {
    credentials::directory(home.as_deref().unwrap_or(configuration))
}

// Asks for whatever the chosen way needs.
fn supply(provider: &provider::Provider, name: &str) -> Result<Credentials, Error> {
    match choose(provider, name)? {
        Way::Key => key(provider),
        Way::Subscription => subscription(provider),
        Way::None => Err(Error::NothingToAuthorize(name.to_owned())),
    }
}

// Asks which way to use when a provider offers more than one worth recording.
//
// A single way is taken rather than asked about, and a provider offering none is a mistake to name
// rather than a question to put.
fn choose(provider: &provider::Provider, name: &str) -> Result<Way, Error> {
    let offered = provider
        .ways()
        .iter()
        .copied()
        .filter(|way| way.is_performable())
        .collect::<Vec<_>>();
    match offered.as_slice() {
        [] => Err(Error::NothingToAuthorize(name.to_owned())),
        [only] => Ok(*only),
        ways => {
            let mut select = cliclack::select("How should this provider be authorized?");
            for way in ways {
                select = select.item(*way, way.id(), hint(*way, provider));
            }

            select.interact().map_err(Error::from)
        }
    }
}

fn hint(way: Way, provider: &provider::Provider) -> String {
    match way {
        Way::Key => provider.key_variables().join(", "),
        Way::Subscription => format!("already signed in to {}", provider.adapter()),
        Way::None => String::new(),
    }
}

// Records that the agent application holds the sign-in.
//
// Nothing to ask for. The operator signs in to that application themselves, and this is the note
// saying they did, so the member is not asked for an API key it does not need.
fn subscription(provider: &provider::Provider) -> Result<Credentials, Error> {
    cliclack::log::info(format!(
        "sign in to {} yourself; this records only that a subscription pays for this member",
        provider.adapter()
    ))?;

    Ok(Credentials::subscription())
}

// Asks for each variable the provider's key way named, so nothing has to be typed from memory.
fn key(provider: &provider::Provider) -> Result<Credentials, Error> {
    let mut variables = BTreeMap::new();
    for variable in provider.key_variables() {
        // An empty value would be stored, reported as authorized, and never asked about again,
        // leaving the member to fail with a 401 on its first delivery.
        let value: String = cliclack::password(variable)
            .mask('*')
            .validate(|value: &String| match value.trim().is_empty() {
                true => Err("a value is required"),
                false => Ok(()),
            })
            .interact()?;
        // Trimmed, because a key pasted from a terminal or a password manager routinely carries a
        // newline, and a stored key with one is a 401 nothing prompts about again.
        let _replaced = variables.insert(variable.clone(), value.trim().to_owned());
    }

    Ok(Credentials::key(variables))
}

/// Error listing or authorizing a provider.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Reading or installing providers failed.
    #[error(transparent)]
    Provider(#[from] provider::Error),
    /// Reading or writing credentials failed.
    #[error(transparent)]
    Credentials(#[from] provider::CredentialsError),
    /// The prompt could not be shown or answered.
    #[error("prompt failed: {0}")]
    Prompt(#[from] std::io::Error),
    /// The provider needs no authorization at all.
    #[error("provider '{0}' needs no authorization")]
    NothingToAuthorize(String),
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::PathBuf};

    use crate::{
        cli::provider::{Error, destination, list, login, status},
        provider::{self, Credentials, Way, credentials},
    };

    // A provider needing nothing and a provider nobody has logged into both have no credentials
    // file, and telling an operator to authorize Ollama would send them looking for a key that does
    // not exist.
    #[test]
    fn separates_a_provider_needing_no_login_from_one_awaiting_it() {
        let local = read("ways = [\"none\"]");
        let keyed = read("ways = [\"key\"]\n\n[auth.key]\nenv = [\"OPENAI_API_KEY\"]");

        assert_eq!(status(&local, None), "none needed");
        assert_eq!(status(&keyed, None), "not authorized");
        assert_eq!(
            status(&keyed, Some(&Credentials::key(BTreeMap::new()))),
            Way::Key.id(),
            "an authorized provider must report how, so a second login is not needed to find out"
        );
    }

    // A login belongs to the machine unless a member was named, which is the whole mechanism for
    // billing one project separately.
    #[test]
    fn records_a_login_against_the_member_when_one_is_named() {
        let configuration = PathBuf::from("/config");
        let home = PathBuf::from("/work/nico");

        assert_eq!(
            destination(&configuration, None),
            configuration.join(credentials::DIRECTORY)
        );
        assert_eq!(
            destination(&configuration, Some(home.clone())),
            home.join(credentials::DIRECTORY)
        );
    }

    // Whatever the shipped set is, `providers` has to be able to print it. Running it is the only
    // way to know that, since a provider which lists but cannot be read prints half a table.
    #[test]
    fn lists_the_providers_it_installs() {
        let configuration = clear("cli-provider-list");

        list(&configuration, None).expect("the shipped providers should list");

        let installed = provider::load(&configuration.join(provider::DIRECTORY))
            .expect("providers should read back");
        assert!(
            !installed.is_empty(),
            "listing must install the shipped providers on a machine which has none"
        );
    }

    // Asking to log into something needing no login is a mistake worth naming, not a file to write.
    // Reached without a prompt, because a single way is chosen rather than asked about.
    #[test]
    fn refuses_to_authorize_a_provider_which_needs_nothing() {
        let configuration = clear("cli-provider-login-none");
        let directory = configuration.join(provider::DIRECTORY);
        std::fs::create_dir_all(&directory).expect("provider directory should be created");
        std::fs::write(
            provider::path(&directory, "local"),
            "adapter = \"opencode\"\n\n[auth]\nways = [\"none\"]\n",
        )
        .expect("provider should be written");

        let error = login(&configuration, "local", None)
            .expect_err("a provider needing nothing must not be authorized");

        assert!(
            matches!(error, Error::NothingToAuthorize(name) if name == "local"),
            "the operator has to be told which provider they need not log into"
        );
    }

    #[test]
    fn reports_a_login_for_a_provider_which_is_not_installed() {
        let configuration = clear("cli-provider-login-unknown");

        let error = login(&configuration, "nowhere", None)
            .expect_err("an absent provider must be reported");

        assert!(matches!(
            error,
            Error::Provider(provider::Error::Unknown(_))
        ));
    }

    // A member-scoped login the listing cannot see reads as never having happened. The operator
    // then logs in again without the flag, putting the project's key in the shared store -- the
    // opposite of what `--home` exists for.
    //
    // Not under Miri: reading a credentials file checks the owner, and Miri reports a user ID of
    // its own which never matches the one that wrote the file.
    #[cfg(not(miri))]
    #[test]
    fn reports_a_login_recorded_against_one_member() {
        let configuration = clear("cli-provider-list-home");
        let home = configuration.join("member");
        let keyed = "openai";
        credentials::store(
            &credentials::directory(&home),
            keyed,
            &Credentials::key(BTreeMap::from([(
                "OPENAI_API_KEY".to_owned(),
                "mine".to_owned(),
            )])),
        )
        .expect("member credentials should be written");

        let machine = credentials::load(&credentials::directory(&configuration), None, keyed)
            .expect("the machine store should read");
        let member = credentials::load(
            &credentials::directory(&configuration),
            Some(&credentials::directory(&home)),
            keyed,
        )
        .expect("the member store should read");

        assert!(
            machine.is_none(),
            "nothing was written to the machine-wide store"
        );
        assert_eq!(
            member.map(|held| held.way()),
            Some(Way::Key),
            "and the listing must be able to see the member's own"
        );
        list(&configuration, Some(&home)).expect("the listing should accept a member home");
    }

    fn clear(name: &str) -> PathBuf {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(name);
        let _removed = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("test directory should be created");

        root
    }

    fn read(auth: &str) -> provider::Provider {
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("cli-provider-status");
        let _removed = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("test directory should be created");
        std::fs::write(
            provider::path(&directory, "some"),
            format!("adapter = \"opencode\"\n\n[auth]\n{auth}\n"),
        )
        .expect("provider should be written");

        provider::find(&directory, "some").expect("provider should be read")
    }
}
