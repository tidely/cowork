//! Connecting to each provider: its client and the API key it needs, kept in
//! the OS keychain through GPUI's credentials API; discovering its models
//! (OpenAI's from a built-in list); and the request parameters only its API
//! takes ([`ModelProvider::request_parameters`]).
//!
//! Keys stay on the host: collaborators run nothing themselves and use the
//! host's catalog, so neither keys nor connections travel.

use std::{collections::HashMap, sync::Arc};

use agent::model::{JsonObject, LanguageModel, LanguageModelProvider, Thinking};
use gpui::{Context, SharedString, Task};
use serde_json::Value;

use crate::{
    Cowork,
    models::{ModelInfo, ModelProvider, Reasoning},
};

/// Where the app finds Ollama.
const OLLAMA_URL: &str = "http://localhost:11434";
const ANTHROPIC_URL: &str = "https://api.anthropic.com/v1";
const OPENAI_URL: &str = "https://api.openai.com/v1";

/// The user name of every key's keychain entry. Entries are found by the
/// provider's URL, one per provider.
const KEYCHAIN_USER: &str = "api-key";

/// Anthropic needs an output cap on every request; this one is for models
/// whose listing doesn't say how long a reply they can write.
const ANTHROPIC_FALLBACK_MAX_OUTPUT: u64 = 32_000;

/// The most an Anthropic model without adaptive thinking may think in one
/// reply. Kept under half the output cap, so thinking leaves room to answer.
const ANTHROPIC_THINKING_BUDGET: u64 = 16_000;

/// The OpenAI models the app offers, as OpenAI's model pages describe them
/// (checked October 2026). OpenAI's listing has neither names nor windows,
/// and lists models for images, audio, and embeddings too, so the app
/// keeps its own list and offers the ones the account's listing includes.
/// Each takes function calling over Chat Completions, which the client
/// uses, with `effort`: GPT-6 Sol and Luna accept tools there only with
/// reasoning off.
const OPENAI_MODELS: &[OpenAiModel] = &[
    OpenAiModel::new("gpt-6-astra", "GPT-6 Astra", 1_050_000, "medium"),
    OpenAiModel::new("gpt-6.1-sol", "GPT-6.1 Sol", 1_050_000, "medium"),
    OpenAiModel::new("gpt-6-sol", "GPT-6 Sol", 1_050_000, "none"),
    OpenAiModel::new("gpt-6-luna", "GPT-6 Luna", 1_050_000, "none"),
    OpenAiModel::new("gpt-5.6-sol", "GPT-5.6 Sol", 1_050_000, "medium"),
    OpenAiModel::new("gpt-5.6-terra", "GPT-5.6 Terra", 1_050_000, "medium"),
    OpenAiModel::new("gpt-5.6-luna", "GPT-5.6 Luna", 1_050_000, "medium"),
    OpenAiModel::new("gpt-5.5", "GPT-5.5", 1_050_000, "medium"),
    OpenAiModel::new("gpt-5.4", "GPT-5.4", 1_050_000, "medium"),
    OpenAiModel::new("gpt-5.4-mini", "GPT-5.4 Mini", 400_000, "medium"),
];

/// One of [`OPENAI_MODELS`]. Every one reads images and writes up to 128K
/// tokens.
struct OpenAiModel {
    id: &'static str,
    name: &'static str,
    context_window: u64,
    effort: &'static str,
}

impl OpenAiModel {
    const MAX_OUTPUT: u64 = 128_000;

    const fn new(
        id: &'static str,
        name: &'static str,
        context_window: u64,
        effort: &'static str,
    ) -> Self {
        Self {
            id,
            name,
            context_window,
            effort,
        }
    }

    fn info(&self) -> ModelInfo {
        ModelInfo {
            name: self.name.into(),
            max_tokens: self.context_window,
            max_output_tokens: Some(Self::MAX_OUTPUT),
            image_input: Some(true),
            reasoning: Reasoning::Effort(self.effort.into()),
        }
    }
}

/// The request parameters a model's provider takes beyond what every
/// provider shares, for the agent definition (see `generation.rs`).
#[derive(Debug, PartialEq)]
pub(crate) struct RequestParameters {
    pub(crate) thinking: Option<Thinking>,
    /// Passed through to the provider's API as they are.
    pub(crate) options: JsonObject,
}

impl ModelProvider {
    /// Whether the provider needs an API key before it lists or runs
    /// models.
    pub(crate) fn needs_api_key(self) -> bool {
        match self {
            Self::Ollama => false,
            Self::Anthropic | Self::OpenAi => true,
        }
    }

    /// Where the user creates an API key, for providers that need one.
    pub(crate) fn api_key_page(self) -> Option<&'static str> {
        match self {
            Self::Ollama => None,
            Self::Anthropic => Some("https://platform.claude.com/settings/keys"),
            Self::OpenAi => Some("https://platform.openai.com/api-keys"),
        }
    }

    fn url(self) -> &'static str {
        match self {
            Self::Ollama => OLLAMA_URL,
            Self::Anthropic => ANTHROPIC_URL,
            Self::OpenAi => OPENAI_URL,
        }
    }

    /// What a request to a model with `info` needs that only this
    /// provider's API takes. Kept apart from the shared definition so every
    /// provider-specific parameter is decided here:
    ///
    /// - Ollama is told the window to run the model with (`num_ctx`).
    /// - Anthropic requires an output cap, and older models think only
    ///   within a budget (`thinking.budget_tokens`).
    /// - OpenAI takes the effort as `reasoning_effort`, and rejects options
    ///   it doesn't know.
    pub(crate) fn request_parameters(self, info: &ModelInfo) -> RequestParameters {
        let effort = match &info.reasoning {
            Reasoning::Effort(effort) => Some(Thinking::Effort(effort.clone())),
            Reasoning::Default | Reasoning::Budget(_) => None,
        };
        let mut options = JsonObject::new();
        match self {
            Self::Ollama => {
                options.insert(
                    "options".into(),
                    serde_json::json!({ "num_ctx": info.max_tokens }),
                );
            }
            Self::Anthropic => {
                let max_output = info
                    .max_output_tokens
                    .unwrap_or(ANTHROPIC_FALLBACK_MAX_OUTPUT);
                options.insert("max_tokens".into(), max_output.into());
                if let Reasoning::Budget(budget) = info.reasoning {
                    options.insert(
                        "thinking".into(),
                        serde_json::json!({ "type": "enabled", "budget_tokens": budget }),
                    );
                }
            }
            Self::OpenAi => {}
        }
        RequestParameters {
            thinking: effort,
            options,
        }
    }
}

/// A connection to `provider`, with `api_key` for those that need one.
pub(crate) fn connect(
    provider: ModelProvider,
    api_key: Option<&str>,
) -> anyhow::Result<Arc<dyn LanguageModelProvider>> {
    let missing_key = || anyhow::anyhow!("{} needs an API key", provider.label());
    Ok(match provider {
        ModelProvider::Ollama => Arc::new(agent::Ollama::new(provider.url())?),
        ModelProvider::Anthropic => Arc::new(
            agent::Anthropic::new(provider.url())?
                .with_api_key(api_key.ok_or_else(missing_key)?)?,
        ),
        ModelProvider::OpenAi => Arc::new(
            agent::OpenAiCompatible::new(provider.url())?
                .with_api_key(api_key.ok_or_else(missing_key)?)?,
        ),
    })
}

/// The app's connections to providers, and what went wrong reaching each.
#[derive(Default)]
pub(crate) struct Providers {
    /// Ollama's connection once made, and each other provider's while it
    /// has a key. Replaced, never changed, when a key changes, so a session
    /// can tell its connection is out of date by comparing pointers.
    connections: HashMap<ModelProvider, Arc<dyn LanguageModelProvider>>,
    /// Why the provider's models couldn't be listed, or its key saved,
    /// until it next succeeds.
    errors: HashMap<ModelProvider, SharedString>,
}

impl Providers {
    /// The connection to `provider`, or why there is none.
    pub(crate) fn connection(
        &mut self,
        provider: ModelProvider,
    ) -> anyhow::Result<Arc<dyn LanguageModelProvider>> {
        if let Some(connection) = self.connections.get(&provider) {
            return Ok(connection.clone());
        }
        anyhow::ensure!(
            !provider.needs_api_key(),
            "{} has no API key. Add one in Settings → Providers.",
            provider.label()
        );
        let connection = connect(provider, None)?;
        self.connections.insert(provider, connection.clone());
        Ok(connection)
    }

    /// Whether `provider` has the API key it needs.
    pub(crate) fn has_api_key(&self, provider: ModelProvider) -> bool {
        provider.needs_api_key() && self.connections.contains_key(&provider)
    }

    pub(crate) fn error(&self, provider: ModelProvider) -> Option<SharedString> {
        self.errors.get(&provider).cloned()
    }

    /// Whether `connection` is still `provider`'s, so what it found applies.
    fn is_current(
        &self,
        provider: ModelProvider,
        connection: &Arc<dyn LanguageModelProvider>,
    ) -> bool {
        self.connections
            .get(&provider)
            .is_some_and(|current| Arc::ptr_eq(current, connection))
    }
}

/// Ollama's models, run with the configured `window` as Ollama has no
/// window of its own to report, at medium effort.
fn ollama_models(models: Vec<LanguageModel>, window: u64) -> Vec<(String, ModelInfo)> {
    models
        .into_iter()
        .filter(|model| !model.id.is_empty())
        .map(|model| {
            let info = ModelInfo {
                name: model.name.unwrap_or_else(|| model.id.clone()),
                max_tokens: window,
                max_output_tokens: None,
                image_input: model.image_input,
                reasoning: Reasoning::Effort("medium".into()),
            };
            (model.id, info)
        })
        .collect()
}

/// An Anthropic model as its listing describes it. Its capabilities decide
/// the reasoning: adaptive thinking at medium effort where the model takes
/// both, otherwise thinking within a budget where it takes that, otherwise
/// none asked for.
fn anthropic_model(model: LanguageModel) -> (String, ModelInfo) {
    let supported = |path: &[&str]| {
        path.iter()
            .try_fold(model.metadata.get("capabilities")?, |value, key| {
                value.get(key)
            })?
            .get("supported")
            .and_then(Value::as_bool)
    };
    let max_output = model
        .max_output_tokens
        .unwrap_or(ANTHROPIC_FALLBACK_MAX_OUTPUT);
    let reasoning = if supported(&["thinking", "types", "adaptive"]) == Some(true)
        && supported(&["effort", "medium"]) == Some(true)
    {
        Reasoning::Effort("medium".into())
    } else if supported(&["thinking", "types", "enabled"]) == Some(true) {
        Reasoning::Budget(ANTHROPIC_THINKING_BUDGET.min(max_output / 2))
    } else {
        Reasoning::Default
    };
    let info = ModelInfo {
        name: model.name.clone().unwrap_or_else(|| model.id.clone()),
        // Every Claude model reads at least this much.
        max_tokens: model.context_window.unwrap_or(200_000),
        max_output_tokens: Some(max_output),
        image_input: supported(&["image_input"]),
        reasoning,
    };
    (model.id, info)
}

/// The built-in OpenAI models the account's listing includes, in the
/// built-in list's order.
fn openai_models<'a>(listed: impl IntoIterator<Item = &'a str>) -> Vec<(String, ModelInfo)> {
    let listed = listed.into_iter().collect::<std::collections::HashSet<_>>();
    OPENAI_MODELS
        .iter()
        .filter(|model| listed.contains(model.id))
        .map(|model| (model.id.to_owned(), model.info()))
        .collect()
}

impl Cowork {
    /// Reads the providers' API keys from the keychain, then discovers
    /// every provider's models. Finishes once discovery has.
    pub(crate) fn start_providers(&mut self, cx: &mut Context<Self>) -> Task<()> {
        let reads = ModelProvider::ALL
            .into_iter()
            .filter(|provider| provider.needs_api_key())
            .map(|provider| (provider, cx.read_credentials(provider.url())))
            .collect::<Vec<_>>();
        cx.spawn(async move |this, cx| {
            for (provider, read) in reads {
                let key = match read.await {
                    Ok(Some((_, key))) => key,
                    Ok(None) => continue,
                    Err(error) => {
                        eprintln!("could not read the {} API key: {error:#}", provider.label());
                        continue;
                    }
                };
                let connected = String::from_utf8(key)
                    .map_err(anyhow::Error::from)
                    .and_then(|key| connect(provider, Some(&key)));
                _ = this.update(cx, |this, _| match connected {
                    Ok(connection) => {
                        this.providers.connections.insert(provider, connection);
                    }
                    Err(error) => {
                        let error = format!("The saved API key can't be used: {error:#}");
                        this.providers.errors.insert(provider, error.into());
                    }
                });
            }
            if let Ok(discovered) = this.update(cx, |this, cx| this.discover_models(cx)) {
                discovered.await;
            }
        })
    }

    /// Discovers every provider's models again. The picker follows on the
    /// next frame; see [`Cowork::sync_model_picker`].
    pub(crate) fn refresh_models(&mut self, cx: &mut Context<Self>) {
        self.discover_models(cx).detach();
    }

    /// [`Cowork::refresh_models`], finishing once every provider's catalog
    /// is replaced or its discovery failed. Providers without a key are
    /// skipped.
    pub(crate) fn discover_models(&mut self, cx: &mut Context<Self>) -> Task<()> {
        let reachable = ModelProvider::ALL
            .into_iter()
            .filter(|&provider| !provider.needs_api_key() || self.providers.has_api_key(provider))
            .collect::<Vec<_>>();
        let discoveries = reachable
            .into_iter()
            .map(|provider| self.discover_provider_models(provider, cx))
            .collect::<Vec<_>>();
        cx.spawn(async move |_, _| {
            futures::future::join_all(discoveries).await;
        })
    }

    /// Asks `provider` which models it offers, and makes them its part of
    /// the catalog.
    fn discover_provider_models(
        &mut self,
        provider: ModelProvider,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let connection = match self.providers.connection(provider) {
            Ok(connection) => connection,
            Err(error) => {
                eprintln!("could not connect to {}: {error:#}", provider.label());
                return Task::ready(());
            }
        };
        let listing = connection.clone();
        let task = self
            .tokio_handle
            .spawn(async move { listing.list_models().await });
        cx.spawn(async move |this, cx| {
            let listed = task
                .await
                .map_err(anyhow::Error::from)
                .and_then(|listed| listed.map_err(anyhow::Error::from));
            _ = this.update(cx, |this, cx| {
                // A key removed or replaced meanwhile makes this out of date.
                if !this.providers.is_current(provider, &connection) {
                    return;
                }
                match listed {
                    Ok(listed) => {
                        let models = match provider {
                            ModelProvider::Ollama => {
                                ollama_models(listed, this.context_settings.window_tokens)
                            }
                            ModelProvider::Anthropic => {
                                listed.into_iter().map(anthropic_model).collect()
                            }
                            ModelProvider::OpenAi => {
                                openai_models(listed.iter().map(|model| model.id.as_str()))
                            }
                        };
                        this.providers.errors.remove(&provider);
                        this.set_provider_models(provider, models, cx);
                    }
                    Err(error) => {
                        eprintln!("could not discover {} models: {error:#}", provider.label());
                        let error = format!("Couldn't list {} models: {error:#}", provider.label());
                        this.providers.errors.insert(provider, error.into());
                    }
                }
                cx.notify();
            });
        })
    }

    /// Makes `models` everything `provider` offers, keeping the other
    /// providers' models.
    fn set_provider_models(
        &mut self,
        provider: ModelProvider,
        models: Vec<(String, ModelInfo)>,
        cx: &mut Context<Self>,
    ) {
        let mut catalog = (*self.models).clone();
        catalog.set_provider(provider, models);
        self.set_models(catalog, cx);
    }

    /// Uses `key` for `provider` from now on, saving it in the keychain,
    /// and discovers the provider's models with it.
    pub(crate) fn save_api_key(
        &mut self,
        provider: ModelProvider,
        key: &str,
        cx: &mut Context<Self>,
    ) {
        let key = key.trim();
        if key.is_empty() || !provider.needs_api_key() {
            return;
        }
        match connect(provider, Some(key)) {
            Ok(connection) => {
                self.providers.connections.insert(provider, connection);
                self.providers.errors.remove(&provider);
                let write = cx.write_credentials(provider.url(), KEYCHAIN_USER, key.as_bytes());
                cx.spawn(async move |this, cx| {
                    if let Err(error) = write.await {
                        _ = this.update(cx, |this, cx| {
                            let error = format!(
                                "The key works until Cowork quits, but couldn't be saved \
                                 in the keychain: {error:#}"
                            );
                            this.providers.errors.insert(provider, error.into());
                            cx.notify();
                        });
                    }
                })
                .detach();
                self.discover_provider_models(provider, cx).detach();
            }
            Err(error) => {
                let error = format!("This key can't be used: {error:#}");
                self.providers.errors.insert(provider, error.into());
            }
        }
        cx.notify();
    }

    /// Forgets `provider`'s key, here and in the keychain. Its models leave
    /// the catalog, so threads using one show it unavailable.
    pub(crate) fn remove_api_key(&mut self, provider: ModelProvider, cx: &mut Context<Self>) {
        if !provider.needs_api_key() {
            return;
        }
        self.providers.connections.remove(&provider);
        self.providers.errors.remove(&provider);
        let delete = cx.delete_credentials(provider.url());
        cx.spawn(async move |_, _| {
            if let Err(error) = delete.await {
                eprintln!(
                    "could not delete the {} API key: {error:#}",
                    provider.label()
                );
            }
        })
        .detach();
        self.set_provider_models(provider, Vec::new(), cx);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(reasoning: Reasoning, max_output_tokens: Option<u64>) -> ModelInfo {
        ModelInfo {
            name: "Model".into(),
            max_tokens: 8_192,
            max_output_tokens,
            image_input: None,
            reasoning,
        }
    }

    #[test]
    fn each_provider_gets_only_its_own_parameters() {
        let effort = Reasoning::Effort("medium".into());
        let medium = Some(Thinking::Effort("medium".into()));
        let ollama = ModelProvider::Ollama.request_parameters(&info(effort.clone(), None));
        assert_eq!(ollama.thinking, medium);
        assert_eq!(
            Value::Object(ollama.options),
            serde_json::json!({ "options": { "num_ctx": 8_192 } })
        );

        let openai = ModelProvider::OpenAi.request_parameters(&info(effort.clone(), Some(128_000)));
        assert_eq!(openai.thinking, medium);
        assert!(
            openai.options.is_empty(),
            "OpenAI rejects unknown parameters"
        );

        let adaptive = ModelProvider::Anthropic.request_parameters(&info(effort, Some(64_000)));
        assert_eq!(adaptive.thinking, medium);
        assert_eq!(
            Value::Object(adaptive.options),
            serde_json::json!({ "max_tokens": 64_000 })
        );
        let budget =
            ModelProvider::Anthropic.request_parameters(&info(Reasoning::Budget(16_000), None));
        assert_eq!(budget.thinking, None);
        assert_eq!(
            Value::Object(budget.options),
            serde_json::json!({
                "max_tokens": ANTHROPIC_FALLBACK_MAX_OUTPUT,
                "thinking": { "type": "enabled", "budget_tokens": 16_000 },
            })
        );
    }

    #[test]
    fn anthropic_capabilities_decide_the_reasoning() {
        let model = |capabilities: Value| {
            let mut model = LanguageModel::new("claude");
            model.max_output_tokens = Some(20_000);
            model.metadata.insert("capabilities".into(), capabilities);
            anthropic_model(model).1
        };
        let yes = serde_json::json!({ "supported": true });
        let no = serde_json::json!({ "supported": false });

        let adaptive = model(serde_json::json!({
            "thinking": { "types": { "adaptive": yes, "enabled": yes } },
            "effort": { "medium": yes },
            "image_input": yes,
        }));
        assert_eq!(adaptive.reasoning, Reasoning::Effort("medium".into()));
        assert_eq!(adaptive.image_input, Some(true));
        assert_eq!(adaptive.max_output_tokens, Some(20_000));

        let budget = model(serde_json::json!({
            "thinking": { "types": { "adaptive": no, "enabled": yes } },
        }));
        // Half the output cap, as it is under the usual budget.
        assert_eq!(budget.reasoning, Reasoning::Budget(10_000));
        assert_eq!(budget.image_input, None);

        assert_eq!(model(Value::Null).reasoning, Reasoning::Default);
    }

    #[test]
    fn openai_offers_the_built_in_models_the_account_lists() {
        let models = openai_models(["gpt-5.4-mini", "text-embedding-3-large", "gpt-6-sol"]);
        let ids = models.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>();
        assert_eq!(ids, ["gpt-6-sol", "gpt-5.4-mini"]);
        assert_eq!(models[0].1.reasoning, Reasoning::Effort("none".into()));
        assert_eq!(models[1].1.max_tokens, 400_000);
    }
}
