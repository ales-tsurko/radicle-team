//! Which models OpenCode is offering, and what each one can be asked for.
//!
//! OpenCode lists a model only once it is authorized for that model's provider, and it derives each
//! model's variants -- its reasoning efforts -- from the registry rather than from anything written
//! here. Both are read back from OpenCode itself, so a provider file naming a model that is not
//! there, or an effort that model does not take, is caught before any work arrives.

use std::collections::BTreeMap;

use crate::{
    agent::{opencode::Error, session},
    sandbox::{AgentCommand, Output},
};

// Every effort OpenCode knows, weakest first. A model offers some run of these, and which run
// differs between models of one provider, so a score has to be fitted to what its own model takes.
const EFFORTS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// Models and reasoning variants returned by the plugin RPC.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Catalogue {
    models: BTreeMap<String, Vec<String>>,
}

impl Catalogue {
    /// Waits for provider activation, then queries the configured model through the plugin RPC.
    pub(super) async fn query(command: AgentCommand, model: &str) -> Result<Self, Error> {
        let listed = Box::pin(
            command
                .args([
                    "api",
                    "--standalone",
                    "rpc.call",
                    "--param",
                    "rpcID=radicle-team",
                    "--param",
                    "method=models",
                    "--data",
                ])
                .arg(serde_json::json!({"input": {"model": model}}).to_string())
                .output(),
        )
        .await?;

        Self::checked(&listed, model)
    }

    fn checked(listed: &Output, model: &str) -> Result<Self, Error> {
        if !listed.status.success() {
            return Err(Error::CatalogueUnavailable {
                status: listed.status,
                detail: session::shorten(
                    format!("{} {}", listed.stderr.trim(), listed.stdout.trim()).trim(),
                ),
            });
        }
        let catalogue = Self::read(&listed.stdout)?;
        if !catalogue.offers(model) {
            return Err(Error::Model(model.to_owned()));
        }

        Ok(catalogue)
    }

    /// Reads the plugin RPC's OpenCode 2 models, whose variants are objects in a list.
    pub(super) fn read(response: &str) -> Result<Self, serde_json::Error> {
        let response: Listed = serde_json::from_str(response)?;

        Ok(Self {
            models: response
                .output
                .into_iter()
                .filter(|model| model.enabled)
                .map(|model| {
                    (
                        format!("{}/{}", model.provider_id, model.id),
                        model
                            .variants
                            .into_iter()
                            .map(|variant| variant.id)
                            .collect(),
                    )
                })
                .collect(),
        })
    }

    /// Returns whether OpenCode offers `model` to this member.
    pub(super) fn offers(&self, model: &str) -> bool {
        self.models.contains_key(model)
    }

    /// Returns the effort to ask `model` for when a member's score asked for `wanted`.
    ///
    /// The score is a member's character, and the efforts a model takes are the model's business,
    /// so the two are fitted rather than one refusing the other: the effort itself when the model
    /// takes it, else the nearest one below it, else the least the model will do. A model with no
    /// efforts at all is asked for none and runs at its own default, which is how a ladder can
    /// climb from a model without reasoning to one with it.
    pub(super) fn effort(&self, model: &str, wanted: &str) -> Option<String> {
        let offered = self.models.get(model)?;
        if offered.iter().any(|effort| effort == wanted) {
            return Some(wanted.to_owned());
        }
        // An effort nobody ranks cannot be fitted to anything, so it is dropped rather than guessed
        // at. The provider file has named something this model does not know.
        let rank = Self::rank(wanted)?;

        offered
            .iter()
            .filter(|effort| Self::rank(effort).is_some_and(|offered| offered < rank))
            .max_by_key(|effort| Self::rank(effort))
            .or_else(|| offered.iter().min_by_key(|effort| Self::rank(effort)))
            .cloned()
    }

    fn rank(effort: &str) -> Option<usize> {
        EFFORTS.iter().position(|known| *known == effort)
    }
}

#[derive(Debug, serde::Deserialize)]
struct Listed {
    output: Vec<Model>,
}

#[derive(Debug, serde::Deserialize)]
struct Model {
    id: String,
    #[serde(rename = "providerID")]
    provider_id: String,
    enabled: bool,
    variants: Vec<Variant>,
}

#[derive(Debug, serde::Deserialize)]
struct Variant {
    id: String,
}

#[cfg(test)]
mod tests {
    use crate::{
        agent::opencode::{Error, catalogue::Catalogue},
        sandbox::Output,
    };

    const LISTING: &str = r#"{"output":[
        {"providerID":"deepseek","id":"deepseek-chat","enabled":true,"variants":[]},
        {"providerID":"deepseek","id":"deepseek-v4-pro","enabled":true,
         "variants":[{"id":"high"},{"id":"max"}]},
        {"providerID":"openai","id":"gpt-5.6-sol","enabled":true,
         "variants":[{"id":"none"},{"id":"low"},{"id":"medium"},{"id":"high"},
                     {"id":"xhigh"},{"id":"max"}]}
    ]}"#;

    #[test]
    fn reads_back_what_opencode_offers() {
        let catalogue = Catalogue::read(LISTING).expect("model catalogue");

        assert!(catalogue.offers("deepseek/deepseek-v4-pro"));
        assert!(catalogue.offers("openai/gpt-5.6-sol"));
        assert!(
            !catalogue.offers("deepseek/deepseek-v4-flash"),
            "a model OpenCode did not list is one this member cannot run"
        );
    }

    #[test]
    fn reads_models_and_their_variants() {
        let catalogue = Catalogue::read(
            r#"{"output":[
            {"providerID":"p","id":"thinking","enabled":true,
             "variants":[{"id":"high","body":{"reasoning_effort":"high"}},{"id":"max"}]},
            {"providerID":"p","id":"plain","enabled":true,"variants":[]},
            {"providerID":"p","id":"disabled","enabled":false,"variants":[]}
        ]}"#,
        )
        .expect("model catalogue");

        assert!(catalogue.offers("p/thinking"));
        assert!(!catalogue.offers("p/disabled"));
        assert_eq!(
            catalogue.effort("p/thinking", "medium").as_deref(),
            Some("high")
        );
        assert_eq!(catalogue.effort("p/plain", "high"), None);
        Catalogue::read(r#"{"error":"unavailable"}"#).expect_err("invalid catalogue");
    }

    // The ladder a provider file climbs can start below the first model that reasons at all, so
    // fitting has to cover a model with every effort, a model with two, and a model with none.
    #[test]
    fn fits_a_members_score_to_what_its_own_model_takes() {
        let catalogue = Catalogue::read(LISTING).expect("model catalogue");

        assert_eq!(
            catalogue.effort("openai/gpt-5.6-sol", "medium").as_deref(),
            Some("medium"),
            "a model which takes the effort asked for must be asked for exactly that"
        );
        assert_eq!(
            catalogue
                .effort("deepseek/deepseek-v4-pro", "medium")
                .as_deref(),
            Some("high"),
            "a model whose least effort is above the score must be asked for its least"
        );
        assert_eq!(
            catalogue
                .effort("deepseek/deepseek-v4-pro", "xhigh")
                .as_deref(),
            Some("high"),
            "a score above everything the model takes must come down to the nearest below it"
        );
        assert_eq!(
            catalogue.effort("deepseek/deepseek-chat", "high"),
            None,
            "a model with no efforts must be asked for none rather than sent one it rejects"
        );
        assert_eq!(
            catalogue.effort("openai/gpt-5.6-sol", "thorough"),
            None,
            "an effort nobody ranks cannot be fitted, and guessing would send an unknown one"
        );
        assert_eq!(catalogue.effort("elsewhere/absent", "high"), None);
    }

    #[test]
    fn rejects_an_unavailable_model_at_startup() {
        for listing in [LISTING, r#"{"output":[]}"#] {
            assert!(matches!(
                Catalogue::checked(&output(0, listing, ""), "p/missing"),
                Err(Error::Model(model)) if model == "p/missing"
            ));
        }
        let catalogue = Catalogue::checked(&output(0, LISTING, ""), "deepseek/deepseek-v4-pro")
            .expect("an offered model can start");
        assert_eq!(
            catalogue
                .effort("deepseek/deepseek-v4-pro", "high")
                .as_deref(),
            Some("high")
        );
    }

    #[test]
    fn rejects_malformed_model_api_responses() {
        for response in ["not JSON", "{}"] {
            assert!(matches!(
                Catalogue::checked(&output(0, response, ""), "p/model"),
                Err(Error::Catalogue(_))
            ));
        }
    }

    #[test]
    fn preserves_the_cli_and_api_explanations_when_a_query_fails() {
        let failure = output(
            1,
            r#"{"error":"provider unavailable"}"#,
            " request failed\n",
        );
        assert!(matches!(
            Catalogue::checked(&failure, "p/model"),
            Err(Error::CatalogueUnavailable { status, detail })
                if status.code() == Some(1)
                    && detail == r#"request failed {"error":"provider unavailable"}"#
        ));
    }

    fn output(code: i32, stdout: &str, stderr: &str) -> Output {
        Output {
            status: std::os::unix::process::ExitStatusExt::from_raw(code << 8),
            stdout: stdout.to_owned(),
            stderr: stderr.to_owned(),
        }
    }
}
