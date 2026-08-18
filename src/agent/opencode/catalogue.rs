//! Which models OpenCode is offering, and what each one can be asked for.
//!
//! OpenCode lists a model only once it is authorized for that model's provider, and it derives each
//! model's variants -- its reasoning efforts -- from the registry rather than from anything written
//! here. Both are read back from OpenCode itself, so a provider file naming a model that is not
//! there, or an effort that model does not take, is caught before any work arrives.

use std::collections::BTreeMap;

// Every effort OpenCode knows, weakest first. A model offers some run of these, and which run
// differs between models of one provider, so a score has to be fitted to what its own model takes.
const EFFORTS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// What `opencode models --verbose` reported.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Catalogue {
    models: BTreeMap<String, Vec<String>>,
}

impl Catalogue {
    /// Reads a listing, which names each model and then describes it.
    ///
    /// A model whose description cannot be read keeps its name and loses its variants, because
    /// being offered at all is the part which decides whether a member can start.
    pub(super) fn read(listing: &str) -> Self {
        let mut models = BTreeMap::new();
        let mut named: Option<&str> = None;
        let mut described = String::new();
        for line in listing.lines() {
            if !Self::names_a_model(line) {
                described.push_str(line);
                continue;
            }
            if let Some(name) = named {
                let _replaced = models.insert(name.to_owned(), Self::variants(&described));
            }
            named = Some(line.trim());
            described.clear();
        }
        if let Some(name) = named {
            let _replaced = models.insert(name.to_owned(), Self::variants(&described));
        }

        Self { models }
    }

    /// Returns whether anything was read back at all.
    ///
    /// An empty catalogue is a listing this adapter could not make sense of rather than an OpenCode
    /// offering nothing, so it is reported rather than acted on.
    pub(super) fn is_empty(&self) -> bool {
        self.models.is_empty()
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

    // A model's name is the only thing in the listing written flush against the left margin.
    fn names_a_model(line: &str) -> bool {
        !line.is_empty() && !line.starts_with(char::is_whitespace) && !line.starts_with(['{', '}'])
    }

    fn variants(described: &str) -> Vec<String> {
        serde_json::from_str::<Described>(described)
            .map(|described| described.variants.into_keys().collect())
            .unwrap_or_default()
    }

    fn rank(effort: &str) -> Option<usize> {
        EFFORTS.iter().position(|known| *known == effort)
    }
}

/// The part of a model's description this adapter reads.
#[derive(Debug, serde::Deserialize)]
struct Described {
    #[serde(default)]
    variants: BTreeMap<String, serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use crate::agent::opencode::catalogue::Catalogue;

    // Verbatim from `opencode models --verbose`, trimmed to the fields this reads.
    const LISTING: &str = r#"deepseek/deepseek-chat
{
  "id": "deepseek-chat",
  "providerID": "deepseek",
  "variants": {}
}
deepseek/deepseek-v4-pro
{
  "id": "deepseek-v4-pro",
  "providerID": "deepseek",
  "variants": {
    "high": { "reasoningEffort": "high" },
    "max": { "reasoningEffort": "max" }
  }
}
openai/gpt-5.6-sol
{
  "id": "gpt-5.6-sol",
  "variants": {
    "none": {},
    "low": {},
    "medium": {},
    "high": {},
    "xhigh": {},
    "max": {}
  }
}
"#;

    #[test]
    fn reads_back_what_opencode_offers() {
        let catalogue = Catalogue::read(LISTING);

        assert!(catalogue.offers("deepseek/deepseek-v4-pro"));
        assert!(catalogue.offers("openai/gpt-5.6-sol"));
        assert!(
            !catalogue.offers("deepseek/deepseek-v4-flash"),
            "a model OpenCode did not list is one this member cannot run"
        );
        assert!(!catalogue.is_empty());
        assert!(
            Catalogue::read("").is_empty(),
            "nothing read back is not the same as a model being absent"
        );
    }

    // The ladder a provider file climbs can start below the first model that reasons at all, so
    // fitting has to cover a model with every effort, a model with two, and a model with none.
    #[test]
    fn fits_a_members_score_to_what_its_own_model_takes() {
        let catalogue = Catalogue::read(LISTING);

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
}
