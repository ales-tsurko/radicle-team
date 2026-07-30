//! Local sandbox configuration setup.

use std::{collections::BTreeSet, path::PathBuf};

use directories::BaseDirs;

use crate::{
    cli::setup::Error,
    sandbox::{Config, ExternalNetwork},
};

#[derive(Debug)]
pub(crate) struct SandboxSetup {
    path: PathBuf,
    config: Config,
    exists: bool,
}

impl SandboxSetup {
    pub(super) fn load(path: PathBuf) -> Result<Self, Error> {
        let config = Config::load_optional(&path)?;
        let exists = config.is_some();

        Ok(Self {
            path,
            config: config.unwrap_or_default(),
            exists,
        })
    }

    pub(super) fn initialize(self) -> Result<(), Error> {
        if self.exists {
            Ok(())
        } else {
            self.config.write(&self.path).map_err(Error::from)
        }
    }

    pub(super) fn configure(self) -> Result<(), Error> {
        cliclack::note("Local sandbox configuration", self.path.display())
            .map_err(Error::from)
            .and_then(|()| self.configure_if_selected())
    }

    fn configure_if_selected(mut self) -> Result<(), Error> {
        if self.should_customize()? {
            self.customize()
        } else {
            self.initialize()
        }
    }

    fn customize(&mut self) -> Result<(), Error> {
        self.configure_paths()?;
        self.config
            .set_external_network(self.prompt_external_network()?);
        self.config.write(&self.path)?;
        cliclack::log::success("Sandbox configuration updated").map_err(Error::from)
    }

    fn should_customize(&self) -> Result<bool, Error> {
        let question = if self.exists {
            "Edit sandbox paths and network access?"
        } else {
            "Customize sandbox paths and network access?"
        };

        cliclack::confirm(question)
            .initial_value(false)
            .interact()
            .map_err(Error::from)
    }

    fn configure_paths(&mut self) -> Result<(), Error> {
        self.config.set_allow_read(
            self.prompt_paths("Additional readable paths", self.config.allow_read())?,
        );
        self.config
            .set_deny_read(self.prompt_paths("Denied readable paths", self.config.deny_read())?);
        self.config.set_allow_write(
            self.prompt_paths("Additional writable paths", self.config.allow_write())?,
        );
        self.config
            .set_deny_write(self.prompt_paths("Denied writable paths", self.config.deny_write())?);

        Ok(())
    }

    fn prompt_paths(&self, prompt: &str, current: &[PathBuf]) -> Result<Vec<PathBuf>, Error> {
        let default = current
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let mut input = cliclack::input(format!("{prompt} (one per line)"))
            .placeholder("Leave empty for none")
            .required(false)
            .multiline();
        if !default.is_empty() {
            input = input.default_input(&default);
        }
        let value: String = input.interact()?;

        Self::parse_paths(&value)
    }

    fn parse_paths(value: &str) -> Result<Vec<PathBuf>, Error> {
        value
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(Self::absolute_path)
            .collect::<Result<BTreeSet<_>, _>>()
            .map(BTreeSet::into_iter)
            .map(Iterator::collect)
    }

    fn absolute_path(value: &str) -> Result<PathBuf, Error> {
        let path = if value == "~" {
            Self::user_home()?
        } else if let Some(suffix) = value.strip_prefix("~/") {
            Self::user_home()?.join(suffix)
        } else {
            PathBuf::from(value)
        };
        std::path::absolute(path).map_err(Error::from)
    }

    fn user_home() -> Result<PathBuf, Error> {
        BaseDirs::new()
            .map(|directories| directories.home_dir().to_owned())
            .ok_or(Error::HomeUnavailable)
    }

    fn prompt_external_network(&self) -> Result<ExternalNetwork, Error> {
        let initial = NetworkChoice::from(self.config.external_network());
        let choice = cliclack::select("Sandbox external network access")
            .initial_value(initial)
            .item(NetworkChoice::All, "Allow all", "default")
            .item(
                NetworkChoice::Disabled,
                "Disable external network",
                "loopback stays reachable",
            )
            .item(
                NetworkChoice::Domains,
                "Allow selected external domains",
                "",
            )
            .interact()?;
        match choice {
            NetworkChoice::All => Ok(ExternalNetwork::All),
            NetworkChoice::Disabled => Ok(ExternalNetwork::Disabled),
            NetworkChoice::Domains => self.prompt_domains(),
        }
    }

    fn prompt_domains(&self) -> Result<ExternalNetwork, Error> {
        let default = self.current_domains().join("\n");
        let mut input = cliclack::input("Allowed external domains (one per line)")
            .placeholder("api.openai.com")
            .multiline();
        if !default.is_empty() {
            input = input.default_input(&default);
        }
        let value: String = input.interact()?;
        let domains = Self::parse_domains(&value);
        if domains.is_empty() {
            return Err(Error::EmptySandboxDomains);
        }

        Ok(ExternalNetwork::Domains(domains))
    }

    fn current_domains(&self) -> &[String] {
        match self.config.external_network() {
            ExternalNetwork::Domains(domains) => domains,
            ExternalNetwork::All | ExternalNetwork::Disabled => &[],
        }
    }

    fn parse_domains(value: &str) -> Vec<String> {
        value
            .lines()
            .map(str::trim)
            .filter(|domain| !domain.is_empty())
            .map(str::to_owned)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NetworkChoice {
    All,
    Disabled,
    Domains,
}

impl From<&ExternalNetwork> for NetworkChoice {
    fn from(network: &ExternalNetwork) -> Self {
        match network {
            ExternalNetwork::All => Self::All,
            ExternalNetwork::Disabled => Self::Disabled,
            ExternalNetwork::Domains(_) => Self::Domains,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SandboxSetup;

    #[test]
    fn normalizes_and_deduplicates_domain_lists() {
        assert_eq!(
            SandboxSetup::parse_domains(" api.openai.com\n\ncrates.io\napi.openai.com "),
            ["api.openai.com", "crates.io"]
        );
    }

    #[test]
    fn resolves_relative_paths() {
        let path =
            SandboxSetup::absolute_path("target/cache").expect("relative path should resolve");

        assert!(path.is_absolute());
        assert!(path.ends_with("target/cache"));
    }
}
