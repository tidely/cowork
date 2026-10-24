//! The models a thread can run, grouped by provider, and how a thread refers
//! to one of them.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A provider the app can run models on.
///
/// Providers are known at compile time, so the variant is the provider's
/// identity: a catalog lists each at most once, and a model reference cannot
/// name a provider that does not exist. Its name and icon are fixed per
/// variant (see `ModelProvider::label` in `model_picker.rs`) and never travel over the
/// wire. How the app connects to each, and what its requests need, is in
/// `providers.rs`. A generic OpenAI-compatible endpoint would be one more
/// variant.
///
/// Declaration order is the order providers are listed in. New variants go
/// last: saved settings and schedules name providers, and the wire numbers
/// them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(crate) enum ModelProvider {
    Ollama,
    Anthropic,
    OpenAi,
}

impl ModelProvider {
    /// Providers offered during setup, including those with no discovered models.
    pub(crate) const ALL: [Self; 3] = [Self::Ollama, Self::Anthropic, Self::OpenAi];
}

/// Identifies a model: its provider, and the id that provider knows it by.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct ModelRef {
    pub(crate) provider: ModelProvider,
    pub(crate) id: String,
}

/// What a catalog knows about one of its models.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ModelInfo {
    /// Shown in the picker.
    pub(crate) name: String,
    /// The context window the model is run with.
    pub(crate) max_tokens: u64,
    /// The longest reply the model can write, when its provider said;
    /// requests that need a cap (Anthropic's) ask for this much.
    pub(crate) max_output_tokens: Option<u64>,
    /// Whether the model accepts images, as discovery reported it; `None`
    /// when it didn't say. A model known not to is sent a note in place of
    /// each image (see the agent runtime's model metadata).
    pub(crate) image_input: Option<bool>,
    /// How requests ask the model to reason.
    pub(crate) reasoning: Reasoning,
}

/// How requests ask a model to reason, decided when its provider's models
/// are discovered, as models differ in what they accept: asking a model for
/// a kind of reasoning it doesn't support fails the request. Each provider
/// turns it into its own request parameters (`providers.rs`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Reasoning {
    /// Requests don't mention reasoning, leaving the provider's default.
    Default,
    /// A model-defined effort level, such as `"medium"`, or `"none"` to turn
    /// reasoning off where that is how the model accepts tools.
    Effort(String),
    /// Thinking within a token budget: Anthropic's extended thinking, for
    /// models without adaptive thinking.
    Budget(u64),
}

/// The models a thread can run, grouped by provider.
///
/// Keyed by provider and then by model id, so neither can appear twice
/// however the catalog was built or decoded.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ModelCatalog {
    providers: BTreeMap<ModelProvider, BTreeMap<String, ModelInfo>>,
}

impl ModelCatalog {
    /// Replaces the models `provider` offers. A provider with no models is
    /// left out, so it never shows up as an empty group.
    pub(crate) fn set_provider(
        &mut self,
        provider: ModelProvider,
        models: impl IntoIterator<Item = (String, ModelInfo)>,
    ) {
        let models = models.into_iter().collect::<BTreeMap<_, _>>();
        if models.is_empty() {
            self.providers.remove(&provider);
        } else {
            self.providers.insert(provider, models);
        }
    }

    /// Ollama runs a model with the window it is asked for, so its models
    /// use the configured window; other providers' models keep the window
    /// their provider gave. Publishing this catalog keeps peers aligned.
    pub(crate) fn set_context_window(&mut self, tokens: u64) {
        if let Some(models) = self.providers.get_mut(&ModelProvider::Ollama) {
            for info in models.values_mut() {
                info.max_tokens = tokens;
            }
        }
    }

    pub(crate) fn get(&self, model: &ModelRef) -> Option<&ModelInfo> {
        self.providers.get(&model.provider)?.get(&model.id)
    }

    pub(crate) fn contains(&self, model: &ModelRef) -> bool {
        self.get(model).is_some()
    }

    /// Every provider that offers models, in declaration order, with its
    /// models keyed by id.
    pub(crate) fn providers(
        &self,
    ) -> impl Iterator<Item = (ModelProvider, &BTreeMap<String, ModelInfo>)> {
        self.providers
            .iter()
            .filter(|(_, models)| !models.is_empty())
            .map(|(provider, models)| (*provider, models))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(name: &str) -> ModelInfo {
        ModelInfo {
            name: name.into(),
            max_tokens: 1_024,
            max_output_tokens: None,
            image_input: None,
            reasoning: Reasoning::Default,
        }
    }

    #[test]
    fn providers_without_models_are_left_out() {
        let mut catalog = ModelCatalog::default();
        catalog.set_provider(ModelProvider::Ollama, [("qwen".into(), info("Qwen"))]);
        catalog.set_provider(ModelProvider::Ollama, []);

        assert_eq!(catalog, ModelCatalog::default());
        assert_eq!(catalog.providers().count(), 0);
    }

    #[test]
    fn the_configured_window_applies_only_to_ollama() {
        let mut catalog = ModelCatalog::default();
        catalog.set_provider(ModelProvider::Ollama, [("qwen".into(), info("Qwen"))]);
        catalog.set_provider(ModelProvider::OpenAi, [("gpt".into(), info("GPT"))]);
        catalog.set_context_window(4_096);

        let window = |provider, id: &str| {
            catalog
                .get(&ModelRef {
                    provider,
                    id: id.into(),
                })
                .unwrap()
                .max_tokens
        };
        assert_eq!(window(ModelProvider::Ollama, "qwen"), 4_096);
        assert_eq!(window(ModelProvider::OpenAi, "gpt"), 1_024);
    }
}
