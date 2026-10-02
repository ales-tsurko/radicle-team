//! Listing, signing in, and proposing a provider selection.

use crate::{
    Application,
    agent::authentication::Authentication,
    cli::{
        provider::{Authorization, Error, context::Context},
        setup::{ManifestEntry, ManifestUpdate, RepositoryCheckout},
    },
    provider::{self, credentials},
};

impl Context {
    pub(crate) async fn list(&self) -> Result<(), Error> {
        let configuration = provider::configuration()?;
        let providers = provider::installed(&configuration)?;
        println!("{}\n", provider::directory(&configuration).display());
        for (name, provider) in providers {
            let marker = if self.binding.provider() == Some(name.as_str()) {
                "*"
            } else {
                " "
            };
            match provider {
                Err(error) => println!("{marker} {name:<12} unusable: {error}"),
                Ok(provider) => {
                    let held = credentials::load(
                        &credentials::directory(&configuration),
                        Some(&credentials::directory(self.home.path())),
                        &name,
                    )?;
                    let auth = Authentication::new(&provider, self.member, &self.home);
                    let status = match auth.authorized(held.as_ref()).await {
                        Ok(true) => held
                            .as_ref()
                            .map_or("none needed".into(), |held| held.way().id().to_owned()),
                        Ok(false) => "not authorized".into(),
                        Err(error) => format!("unavailable: {error}"),
                    };
                    println!("{marker} {name:<12} {:<12} {status}", provider.adapter());
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn login(&self) -> Result<(), Error> {
        let name = self.provider()?;
        let configuration = provider::configuration()?;
        let provider = provider::installed_named(&configuration, name)?;
        cliclack::intro(format!("Authorize {name} for {}", self.member))?;
        if provider.needs_authorizing() {
            Authorization::new(&provider, &configuration, name, self.member, &self.home)
                .login()
                .await?;
        }
        cliclack::outro(format!(
            "{name} is ready for all of {}'s roles",
            self.member
        ))?;
        Ok(())
    }

    pub(crate) async fn select(&self) -> Result<(), Error> {
        if !self.canonical {
            return Err(Error::PendingSetup);
        }
        let configuration = provider::configuration()?;
        let providers = provider::installed(&configuration)?;
        cliclack::intro(format!(
            "Select a provider for {} (current: {})",
            self.member,
            self.binding.provider().unwrap_or("none")
        ))?;
        let mut select = cliclack::select("Which provider should this member use?");
        for (name, provider) in &providers {
            if let Ok(provider) = provider {
                let current = self.binding.provider() == Some(name.as_str());
                let label = if current {
                    format!("{name} (current)")
                } else {
                    name.to_owned()
                };
                select = select.item(name.as_str(), label, provider.adapter());
                if current {
                    select = select.initial_value(name.as_str());
                }
            }
        }
        let name = select.interact()?.to_owned();
        let selected = provider::installed_named(&configuration, &name)?;
        Authorization::new(&selected, &configuration, &name, self.member, &self.home)
            .authorize()
            .await?;
        if self.binding.provider() == Some(name.as_str()) {
            cliclack::outro(format!("{name} is already selected"))?;
            return Ok(());
        }
        let application =
            Application::new(self.member, self.repository).with_home(self.home.path());
        let checkout = self.prepare_selection(&application, &name).await?;
        let generation = self.binding.generation();
        if let Some((manifest, _)) = checkout.preview(generation)? {
            cliclack::note("Proposed manifest", manifest)?;
        }
        if !self.confirm_publication()? {
            return Ok(());
        }
        let title = format!("Use {name} for {}", self.member);
        let patch = tokio::task::spawn_blocking(move || {
            checkout.publish(
                generation,
                &title,
                "Change this member's model provider. The current provider remains active until \
                 this manifest change is merged; restart rad-team run afterwards.",
            )
        })
        .await
        .map_err(crate::cli::setup::Error::Task)??;
        cliclack::log::success(format!("Provider patch: {patch}"))?;
        application.announce_manifest_patch().await?;
        cliclack::outro("After the patch merges, restart rad-team run to use the new provider.")?;
        Ok(())
    }

    async fn prepare_selection(
        &self,
        application: &Application,
        name: &str,
    ) -> Result<RepositoryCheckout, Error> {
        let checkout = RepositoryCheckout::open(application).await?;
        let generation = self.binding.generation();
        if checkout.proposed_provider(generation)?.as_deref() != Some(name) {
            checkout.prepare(ManifestUpdate::Provider(ManifestEntry {
                member: self.member,
                did: self.did,
                generation: i64::try_from(generation)
                    .map_err(crate::cli::setup::Error::Generation)?,
                provider: Some(name.into()),
            }))?;
        }
        Ok(checkout)
    }

    fn confirm_publication(&self) -> Result<bool, Error> {
        let publish = cliclack::confirm("Publish this provider change as a Radicle patch?")
            .initial_value(true)
            .interact()?;
        if !publish {
            cliclack::outro(format!(
                "Manifest saved at {} for manual publication. The current provider stays active \
                 until it merges.",
                self.home
                    .worktree(crate::agent::Role::Coder)
                    .join(crate::manifest::FILE_NAME)
                    .display()
            ))?;
        }
        Ok(publish)
    }
}
