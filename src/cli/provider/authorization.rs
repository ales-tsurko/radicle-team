//! Interactive member authorization.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use crate::{
    agent::authentication::Authentication,
    cli::provider::Error,
    home::MemberHome,
    member::Member,
    provider::{Credentials, Provider, Way, credentials},
};

/// Interactive authorization for one provider in a member's home.
#[derive(Debug)]
pub(crate) struct Authorization<'a> {
    provider: &'a Provider,
    configuration: &'a Path,
    name: &'a str,
    member: Member,
    home: &'a MemberHome,
}

impl<'a> Authorization<'a> {
    pub(crate) const fn new(
        provider: &'a Provider,
        configuration: &'a Path,
        name: &'a str,
        member: Member,
        home: &'a MemberHome,
    ) -> Self {
        Self {
            provider,
            configuration,
            name,
            member,
            home,
        }
    }

    pub(crate) async fn login(&self) -> Result<PathBuf, Error> {
        let previous = credentials::load(
            &credentials::directory(self.configuration),
            Some(&credentials::directory(self.home.path())),
            self.name,
        )?;
        let way = match previous
            .as_ref()
            .map(Credentials::way)
            .filter(|way| self.provider.ways().contains(way) && way.is_performable())
        {
            Some(way) => way,
            None => self.choose()?,
        };
        let held = match way {
            Way::Key => self.key()?,
            Way::Subscription => {
                cliclack::log::info(format!(
                    "Sign in to {} for {}",
                    self.provider.adapter(),
                    self.member
                ))?;
                Authentication::new(self.provider, self.member, self.home)
                    .login()
                    .await?;
                Credentials::subscription()
            }
            Way::None => return Err(Error::NothingToAuthorize(self.name.into())),
        };
        Ok(credentials::store(
            &credentials::directory(self.home.path()),
            self.name,
            &held,
        )?)
    }

    pub(crate) async fn authorize(&self) -> Result<(), Error> {
        let held = credentials::load(
            &credentials::directory(self.configuration),
            Some(&credentials::directory(self.home.path())),
            self.name,
        )?;
        if Authentication::new(self.provider, self.member, self.home)
            .authorized(held.as_ref())
            .await?
        {
            return Ok(());
        }
        let path = self.login().await?;
        cliclack::log::success(format!("{} authorized in {}", self.name, path.display()))?;
        Ok(())
    }

    fn choose(&self) -> Result<Way, Error> {
        let offered = self
            .provider
            .ways()
            .iter()
            .copied()
            .filter(|way| way.is_performable())
            .collect::<Vec<_>>();
        match offered.as_slice() {
            [] => Err(Error::NothingToAuthorize(self.name.to_owned())),
            [only] => Ok(*only),
            ways => {
                let mut select = cliclack::select("How should this provider be authorized?");
                for way in ways {
                    select = select.item(*way, way.id(), self.hint(*way));
                }

                select.interact().map_err(Error::from)
            }
        }
    }

    fn hint(&self, way: Way) -> String {
        match way {
            Way::Key => self.provider.key_variables().join(", "),
            Way::Subscription => format!("sign in to {} for this member", self.provider.adapter()),
            Way::None => String::new(),
        }
    }

    // Asks for each variable the provider's key way named, so nothing has to be typed from memory.
    fn key(&self) -> Result<Credentials, Error> {
        let mut variables = BTreeMap::new();
        for variable in self.provider.key_variables() {
            // An empty value would be stored, reported as authorized, and never asked about again,
            // leaving the member to fail with a 401 on its first delivery.
            let value: String = cliclack::password(variable)
                .mask('*')
                .validate(|value: &String| match value.trim().is_empty() {
                    true => Err("a value is required"),
                    false => Ok(()),
                })
                .interact()?;
            // Trimmed, because a key pasted from a terminal or a password manager routinely carries
            // a newline, and a stored key with one is a 401 nothing prompts about
            // again.
            let _replaced = variables.insert(variable.clone(), value.trim().to_owned());
        }

        Ok(Credentials::key(variables))
    }
}
