//! The models a thread can run, grouped by provider, and how a thread refers
//! to one of them.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A provider the app can run models on.
///
/// Providers are known at compile time, so the variant is the provider's
/// identity: a catalog lists each at most once, and a model reference cannot
/// name a provider that does not exist. Its name and icon are fixed per
/// variant (see `ModelProvider::label` in `main.rs`) and never travel over the
/// wire. A generic OpenAI-compatible endpoint would be one more variant.
///
/// Declaration order is the order providers are listed in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(crate) enum ModelProvider {
    Ollama,
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
        }
    }

    fn ollama(id: &str) -> ModelRef {
        ModelRef {
            provider: ModelProvider::Ollama,
            id: id.into(),
        }
    }

    #[test]
    fn models_are_found_by_provider_and_id() {
        let mut catalog = ModelCatalog::default();
        catalog.set_provider(
            ModelProvider::Ollama,
            [
                ("qwen".into(), info("Qwen")),
                ("llama".into(), info("Llama")),
            ],
        );

        assert_eq!(catalog.get(&ollama("qwen")), Some(&info("Qwen")));
        assert!(!catalog.contains(&ollama("missing")));
    }

    #[test]
    fn a_model_id_appears_once_per_provider() {
        let mut catalog = ModelCatalog::default();
        catalog.set_provider(
            ModelProvider::Ollama,
            [
                ("qwen".into(), info("First")),
                ("qwen".into(), info("Second")),
            ],
        );

        let (_, models) = catalog.providers().next().expect("ollama models");
        assert_eq!(models.len(), 1);
        assert_eq!(catalog.get(&ollama("qwen")), Some(&info("Second")));
    }

    #[test]
    fn providers_without_models_are_left_out() {
        let mut catalog = ModelCatalog::default();
        catalog.set_provider(ModelProvider::Ollama, [("qwen".into(), info("Qwen"))]);
        catalog.set_provider(ModelProvider::Ollama, []);

        assert_eq!(catalog, ModelCatalog::default());
        assert_eq!(catalog.providers().count(), 0);
    }
}
