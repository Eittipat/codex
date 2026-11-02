//! Discover models from configured OpenAI-compatible provider endpoints.
use crate::legacy_core::config::Config;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Duration;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ProviderModel {
    pub provider: String,
    pub id: String,
    pub name: String,
}
#[derive(Deserialize)]
struct ModelList {
    data: Vec<Model>,
}
#[derive(Deserialize)]
struct Model {
    id: String,
    name: Option<String>,
}

pub(crate) async fn discover(
    config: &Config,
    openai_models: Vec<codex_protocol::openai_models::ModelPreset>,
    catalog_provider: Option<&str>,
) -> (Vec<ProviderModel>, Vec<String>) {
    let cache_path = config.codex_home.join("provider_models_cache.json");
    let mut cache: BTreeMap<String, (String, Vec<ProviderModel>)> =
        match tokio::fs::read(&cache_path).await {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => BTreeMap::new(),
        };
    let mut models = Vec::new();
    let mut errors = Vec::new();
    for (id, provider) in &config.model_providers {
        if id == "openai" {
            continue;
        }
        // Built-in backends are available in every config, but should be shown only
        // when explicitly selected or configured by the user or a managed layer.
        let built_in = matches!(
            id.as_str(),
            "amazon-bedrock" | "amazon-bedrock-runtime" | "gcp" | "ollama" | "lmstudio"
        );
        let explicitly_configured = config.config_layer_stack.layers_high_to_low().any(|layer| {
            layer
                .config
                .get("model_providers")
                .and_then(|providers| providers.as_table())
                .is_some_and(|providers| providers.contains_key(id))
        });
        if built_in && id != &config.model_provider_id && !explicitly_configured {
            continue;
        }
        if provider.is_amazon_bedrock() || provider.is_amazon_bedrock_runtime() || provider.is_gcp()
        {
            let catalog_provider =
                codex_model_provider::create_model_provider(provider.clone(), None);
            let manager = catalog_provider.models_manager_without_cache(None);
            let presets = manager
                .list_models(
                    codex_models_manager::manager::RefreshStrategy::Offline,
                    config.http_client_factory(),
                )
                .await;
            models.extend(
                presets
                    .into_iter()
                    .filter(|model| model.show_in_picker)
                    .map(|model| ProviderModel {
                        provider: id.clone(),
                        id: model.model,
                        name: model.display_name,
                    }),
            );
            continue;
        }
        let Some(base) = provider.base_url.as_deref() else {
            continue;
        };
        let Ok(url) = url::Url::parse(base) else {
            continue;
        };
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            errors.push(format!(
                "{id}: discovery URL must not include credentials, query, or fragment"
            ));
            continue;
        }
        if url.scheme() != "https"
            && !(url.scheme() == "http"
                && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1")))
        {
            errors.push(format!("{id}: discovery requires HTTPS or localhost"));
            continue;
        }
        let endpoint = format!("{}/models", base.trim_end_matches(char::from(47)));
        let before = models.len();
        let client = match codex_http_client::HttpClientBuilder::new()
            .without_redirects()
            .without_request_logging()
            .build_respecting_outbound_proxy_policy(
                &config.http_client_factory(),
                &endpoint,
                codex_http_client::ClientRouteClass::Other,
            ) {
            Ok(client) => client,
            Err(err) => {
                errors.push(format!("{id}: {err}"));
                continue;
            }
        };
        let key = match provider.api_key() {
            Ok(key) => key,
            Err(err) => {
                errors.push(format!("{id}: {err}"));
                continue;
            }
        };
        let key = key.or_else(|| {
            provider
                .experimental_bearer_token
                .as_ref()
                .map(|token| token.to_string())
        });
        let mut request = client.get(endpoint).timeout(Duration::from_secs(15));
        // Gateway OAuth is outside the configured-provider discovery flow.
        // Never start interactive gateway login while browsing models.
        if provider.gateway_oauth.is_some() {
            errors.push(format!(
                "{id}: gateway OAuth model discovery is not supported"
            ));
            continue;
        }
        if provider.requires_openai_auth || provider.auth.is_some() {
            let authentication = async {
                let manager = codex_login::AuthManager::shared_from_config(config, false)
                    .await
                    .map_err(|err| err.to_string())?;
                let configured =
                    codex_model_provider::create_model_provider(provider.clone(), Some(manager));
                let auth = configured.api_auth().await.map_err(|err| err.to_string())?;
                auth.apply_auth_headers(Default::default())
                    .await
                    .map_err(|err| err.to_string())
            };
            let headers = match tokio::time::timeout(Duration::from_secs(15), authentication).await
            {
                Ok(Ok(headers)) => headers,
                Ok(Err(err)) => {
                    errors.push(format!("{id}: authentication unavailable: {err}"));
                    continue;
                }
                Err(_) => {
                    errors.push(format!("{id}: authentication timed out"));
                    continue;
                }
            };
            request = request.headers(headers);
        }
        if let Some(headers) = &provider.http_headers {
            for (name, value) in headers {
                request = request.header(name.as_str(), value.as_str());
            }
        }
        if let Some(headers) = &provider.env_http_headers {
            for (name, variable) in headers {
                if let Ok(value) = std::env::var(variable) {
                    request = request.header(name.as_str(), value);
                }
            }
        }
        if let Some(key) = key {
            request = request.bearer_auth(key);
        }
        let response = match request.send().await.and_then(|r| r.error_for_status()) {
            Ok(response) => response,
            Err(err) => {
                errors.push(format!("{id}: {err}; using cached models if available"));
                if let Some((cached_base, cached)) = cache.get(id)
                    && cached_base == base
                {
                    models.extend(cached.clone());
                }
                continue;
            }
        };
        let mut fetched = false;
        match response.json::<ModelList>().await {
            Ok(list) => {
                fetched = true;
                models.extend(
                    list.data
                        .into_iter()
                        .filter(|model| !model.id.trim().is_empty())
                        .map(|model| ProviderModel {
                            provider: id.clone(),
                            name: model.name.unwrap_or_else(|| model.id.clone()),
                            id: model.id,
                        }),
                );
            }
            Err(err) => {
                errors.push(format!(
                    "{id}: invalid model list: {err}; using cached models if available"
                ));
                if let Some((cached_base, cached)) = cache.get(id)
                    && cached_base == base
                {
                    models.extend(cached.clone());
                }
            }
        }
        if fetched {
            cache.insert(id.clone(), (base.to_string(), models[before..].to_vec()));
        }
    }
    // Persist catalog entries only when the caller identifies the server catalog as OpenAI.
    if !openai_models.is_empty() {
        let entries: Vec<_> = openai_models
            .into_iter()
            .filter(|model| model.show_in_picker)
            .map(|model| ProviderModel {
                provider: "openai".to_string(),
                id: model.model,
                name: model.display_name,
            })
            .collect();
        cache.insert("openai".to_string(), ("openai".to_string(), entries));
    }
    if config.model_providers.contains_key("openai")
        && !cache.contains_key("openai")
        && let Ok(catalog) = codex_models_manager::bundled_models_response()
    {
        let entries = catalog
            .models
            .into_iter()
            .filter(|model| {
                model.visibility == codex_protocol::openai_models::ModelVisibility::List
            })
            .map(|model| ProviderModel {
                provider: "openai".to_string(),
                id: model.slug,
                name: model.display_name,
            })
            .collect();
        cache.insert("openai".to_string(), ("openai".to_string(), entries));
    }
    if config.model_providers.contains_key("openai")
        && let Some((origin, entries)) = cache.get("openai")
        && origin == "openai"
    {
        models.extend(entries.clone());
    }
    if let Ok(bytes) = serde_json::to_vec(&cache) {
        let _ = tokio::fs::write(cache_path, bytes).await;
    }
    // Include a locally configured Codex catalog for the active provider.
    if catalog_provider == Some(config.model_provider_id.as_str())
        && let Some(catalog) = &config.model_catalog
    {
        models.extend(
            catalog
                .models
                .iter()
                .filter(|model| {
                    model.visibility == codex_protocol::openai_models::ModelVisibility::List
                })
                .map(|model| ProviderModel {
                    provider: config.model_provider_id.clone(),
                    id: model.slug.clone(),
                    name: model.display_name.clone(),
                }),
        );
    }
    models.sort_by(|a, b| (&a.provider, &a.id).cmp(&(&b.provider, &b.id)));
    models.dedup_by(|a, b| a.provider == b.provider && a.id == b.id);
    (models, errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::legacy_core::config::ConfigBuilder;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn discovery_uses_cache_when_provider_is_unavailable() {
        let home = tempfile::tempdir().unwrap();
        let mut config = ConfigBuilder::default()
            .codex_home(home.path().to_path_buf())
            .build()
            .await
            .unwrap();
        config.model_providers.retain(|id, _| id == "openai");
        let server = MockServer::start().await;
        let mut provider = config.model_providers["openai"].clone();
        provider.base_url = Some(server.uri());
        provider.requires_openai_auth = false;
        provider.experimental_bearer_token = Some("test-token".into());
        config.model_providers.insert("example".into(), provider);
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"data":[{"id":"vendor/model","name":"Example model"}]}),
            ))
            .mount(&server)
            .await;
        let (models, errors) = discover(&config, Vec::new(), None).await;
        assert!(errors.is_empty(), "{errors:?}");
        assert!(
            models
                .iter()
                .any(|model| model.provider == "example" && model.id == "vendor/model")
        );
        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let (models, errors) = discover(&config, Vec::new(), None).await;
        assert!(errors.iter().any(|error| error.contains("using cached")));
        assert!(
            models
                .iter()
                .any(|model| model.provider == "example" && model.id == "vendor/model")
        );
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn discovery_uses_command_authentication() {
        let home = tempfile::tempdir().unwrap();
        let mut config = ConfigBuilder::default()
            .codex_home(home.path().to_path_buf())
            .build()
            .await
            .unwrap();
        config.model_providers.retain(|id, _| id == "openai");
        let server = MockServer::start().await;
        let mut provider = config.model_providers["openai"].clone();
        provider.base_url = Some(server.uri());
        provider.requires_openai_auth = false;
        provider.auth = Some(
            toml::from_str(
                "command = \"/usr/bin/printf\"
args = [\"command-token\"]
",
            )
            .unwrap(),
        );
        config
            .model_providers
            .insert("command-provider".into(), provider);
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("authorization", "Bearer command-token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"data":[{"id":"test-model"}]})),
            )
            .mount(&server)
            .await;
        let (models, errors) = discover(&config, Vec::new(), None).await;
        assert!(errors.is_empty(), "{errors:?}");
        assert!(
            models
                .iter()
                .any(|model| model.provider == "command-provider" && model.id == "test-model")
        );
    }
    #[tokio::test]
    async fn discovery_lists_cloud_provider_catalogs_without_network() {
        let home = tempfile::tempdir().unwrap();
        let mut config = ConfigBuilder::default()
            .codex_home(home.path().to_path_buf())
            .build()
            .await
            .unwrap();
        config.model_providers.retain(|id, _| {
            matches!(
                id.as_str(),
                "openai" | "amazon-bedrock" | "amazon-bedrock-runtime" | "gcp"
            )
        });
        let (unconfigured, errors) = discover(&config, Vec::new(), None).await;
        assert!(errors.is_empty(), "{errors:?}");
        assert!(unconfigured.iter().all(|model| model.provider == "openai"));
        for provider in ["amazon-bedrock", "amazon-bedrock-runtime", "gcp"] {
            config.model_provider_id = provider.to_string();
            let (models, errors) = discover(&config, Vec::new(), None).await;
            assert!(errors.is_empty(), "{errors:?}");
            assert!(
                models.iter().any(|model| model.provider == provider),
                "missing catalog for {provider}"
            );
            assert!(
                models
                    .iter()
                    .all(|model| model.provider == provider || model.provider == "openai")
            );
        }
    }

    #[tokio::test]
    async fn explicit_catalog_is_only_attached_to_its_provider() {
        let home = tempfile::tempdir().unwrap();
        let mut config = ConfigBuilder::default()
            .codex_home(home.path().to_path_buf())
            .build()
            .await
            .unwrap();
        config.model_providers.retain(|id, _| id == "openai");
        config.model_provider_id = "example".to_string();
        config.model_catalog = Some(codex_models_manager::bundled_models_response().unwrap());
        let (unmatched, _) = discover(&config, Vec::new(), Some("openai")).await;
        assert!(!unmatched.iter().any(|model| model.provider == "example"));
        let (matched, _) = discover(&config, Vec::new(), Some("example")).await;
        assert!(matched.iter().any(|model| model.provider == "example"));
    }
}
