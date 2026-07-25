//! HTTP proxy client for forwarding requests to Copilot API.

use crate::auth::TokenManager;
use crate::error::Error;
use axum::body::{Body, Bytes};
use axum::response::Response;
use futures::TryStreamExt;
use reqwest::Client;
use reqwest::header::{HeaderMap, HeaderValue};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

const HOP_BY_HOP: &[&str] = &[
    "transfer-encoding",
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "upgrade",
];

const MODELS_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const MODELS_UNKNOWN_REFRESH_TTL: Duration = Duration::from_secs(5 * 60);

struct ModelsCache {
    fetched_at: Instant,
    reasoning_efforts_by_model: HashMap<String, Vec<String>>,
    model_ids: HashSet<String>,
}

pub struct ProxyClient {
    client: Client,
    token_manager: Arc<TokenManager>,
    device_id: String,
    machine_id: String,
    session_id: String,
    models_cache: RwLock<Option<ModelsCache>>,
}

impl ProxyClient {
    pub fn new(token_manager: Arc<TokenManager>) -> Result<Self, Error> {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()?;
        let device_id = crate::config::load_vscode_device_id();
        let machine_id = crate::config::load_vscode_machine_id();
        let session_id = uuid::Uuid::new_v4().to_string();
        Ok(Self {
            client,
            token_manager,
            device_id,
            machine_id,
            session_id,
            models_cache: RwLock::new(None),
        })
    }

    pub async fn forward(
        &self,
        path: &str,
        method: reqwest::Method,
        body: Bytes,
        content_type: Option<&str>,
        initiator: Option<&str>,
        is_vision: bool,
    ) -> Result<reqwest::Response, Error> {
        self.forward_inner(path, method, body, content_type, initiator, is_vision, true)
            .await
    }

    async fn forward_inner(
        &self,
        path: &str,
        method: reqwest::Method,
        body: Bytes,
        content_type: Option<&str>,
        initiator: Option<&str>,
        is_vision: bool,
        track_upstream: bool,
    ) -> Result<reqwest::Response, Error> {
        let token = self.token_manager.get_token().await?;
        let api_base = self.token_manager.get_api_base().await?;

        let resp = self
            .send_request(
                &api_base,
                path,
                method.clone(),
                &body,
                content_type,
                &token,
                initiator,
                is_vision,
                track_upstream,
            )
            .await?;

        // On 401, force-refresh the Copilot token and retry once (handles sleep/wake expiry)
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            tracing::warn!("Received 401 from upstream, attempting token refresh and retry");
            if self.token_manager.force_refresh(&token).await.is_ok() {
                let new_token = self.token_manager.get_token().await?;
                let new_api_base = self.token_manager.get_api_base().await?;
                return self
                    .send_request(
                        &new_api_base,
                        path,
                        method,
                        &body,
                        content_type,
                        &new_token,
                        initiator,
                        is_vision,
                        track_upstream,
                    )
                    .await;
            }
        }

        Ok(resp)
    }

    pub async fn fetch_usage(&self) -> Result<reqwest::Response, Error> {
        let github_token = &self.token_manager.github_token;
        let resp = self
            .client
            .get("https://api.github.com/copilot_internal/user")
            .header("Authorization", format!("token {}", github_token))
            .header("Accept", "application/json")
            .header("editor-version", "vscode/1.114.0")
            .header("editor-plugin-version", "copilot-chat/0.26.7")
            .header("user-agent", "GitHubCopilotChat/0.26.7")
            .header("x-github-api-version", "2026-01-09")
            .send()
            .await?;
        Ok(resp)
    }

    pub async fn supported_reasoning_efforts(
        &self,
        model: &str,
    ) -> Result<Option<Vec<String>>, Error> {
        let keys = model_lookup_keys(model);
        if let Some(efforts) = self.cached_reasoning_efforts(&keys).await {
            return Ok(efforts);
        }

        let (reasoning_efforts_by_model, model_ids) = self.fetch_models_data().await?;
        let efforts = keys
            .iter()
            .find_map(|key| reasoning_efforts_by_model.get(key).cloned());
        *self.models_cache.write().await = Some(ModelsCache {
            fetched_at: Instant::now(),
            reasoning_efforts_by_model,
            model_ids,
        });
        Ok(efforts)
    }

    /// Returns `Some(true)` when Copilot upstream is known to expose `model`,
    /// `Some(false)` when the model list was fetched but does not contain it,
    /// and `None` when the model list could not be determined.
    pub async fn is_supported_model(&self, model: &str) -> Option<bool> {
        let keys = model_lookup_keys(model);
        {
            let cache = self.models_cache.read().await;
            if let Some(cache) = cache.as_ref()
                && cache.fetched_at.elapsed() <= MODELS_CACHE_TTL
            {
                return Some(keys.iter().any(|key| cache.model_ids.contains(key)));
            }
        }

        let (reasoning_efforts_by_model, model_ids) = self.fetch_models_data().await.ok()?;
        let supported = keys.iter().any(|key| model_ids.contains(key));
        *self.models_cache.write().await = Some(ModelsCache {
            fetched_at: Instant::now(),
            reasoning_efforts_by_model,
            model_ids,
        });
        Some(supported)
    }

    async fn cached_reasoning_efforts(&self, keys: &[String]) -> Option<Option<Vec<String>>> {
        let cache = self.models_cache.read().await;
        let cache = cache.as_ref()?;
        cached_reasoning_efforts_from_cache(cache, keys)
    }

    async fn fetch_models_data(
        &self,
    ) -> Result<(HashMap<String, Vec<String>>, HashSet<String>), Error> {
        let resp = self
            .forward_inner(
                "/models",
                reqwest::Method::GET,
                Bytes::new(),
                None,
                None,
                false,
                false,
            )
            .await?;
        if !resp.status().is_success() {
            return Err(Error::InvalidRequest(format!(
                "failed to fetch Copilot models: HTTP {}",
                resp.status()
            )));
        }

        let value: serde_json::Value = resp.json().await?;
        let mut map = HashMap::new();
        let mut ids = HashSet::new();
        let Some(models) = value.get("data").and_then(|v| v.as_array()) else {
            return Ok((map, ids));
        };

        for model in models {
            for pointer in ["/id", "/version", "/capabilities/family"] {
                if let Some(key) = model.pointer(pointer).and_then(|v| v.as_str()) {
                    for key in model_lookup_keys(key) {
                        ids.insert(key);
                    }
                }
            }

            let Some(efforts) = model
                .pointer("/capabilities/supports/reasoning_effort")
                .and_then(|v| v.as_array())
                .map(|values| {
                    values
                        .iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect::<Vec<_>>()
                })
                .filter(|values| !values.is_empty())
            else {
                continue;
            };

            for pointer in ["/id", "/version", "/capabilities/family"] {
                if let Some(key) = model.pointer(pointer).and_then(|v| v.as_str()) {
                    for key in model_lookup_keys(key) {
                        map.insert(key, efforts.clone());
                    }
                }
            }
        }

        Ok((map, ids))
    }

    async fn send_request(
        &self,
        api_base: &str,
        path: &str,
        method: reqwest::Method,
        body: &Bytes,
        content_type: Option<&str>,
        token: &str,
        initiator: Option<&str>,
        is_vision: bool,
        track_upstream: bool,
    ) -> Result<reqwest::Response, Error> {
        if track_upstream {
            let resolved_initiator = if initiator == Some("agent") {
                "agent"
            } else {
                "user"
            };
            crate::server::record_upstream(resolved_initiator, path);
        }

        let mut req = self
            .client
            .request(method, format!("{}{}", api_base, path))
            .bearer_auth(token)
            .headers(copilot_headers(
                &self.device_id,
                &self.machine_id,
                &self.session_id,
                initiator,
                is_vision,
            ));

        if let Some(ct) = content_type {
            req = req.header("Content-Type", ct);
        }

        Ok(req.body(body.clone()).send().await?)
    }
}

fn cached_reasoning_efforts_from_cache(
    cache: &ModelsCache,
    keys: &[String],
) -> Option<Option<Vec<String>>> {
    let elapsed = cache.fetched_at.elapsed();
    if elapsed > MODELS_CACHE_TTL {
        return None;
    }

    let cached = keys
        .iter()
        .find_map(|key| cache.reasoning_efforts_by_model.get(key).cloned());
    if cached.is_some() || elapsed <= MODELS_UNKNOWN_REFRESH_TTL {
        return Some(cached);
    }

    None
}

fn model_lookup_keys(model: &str) -> Vec<String> {
    let key = model.to_lowercase();
    let mut keys = vec![key.clone()];

    let dashed_key = key.replace('.', "-");
    if dashed_key != key {
        keys.push(dashed_key);
    }

    if let Some(dotted_key) = dot_last_numeric_dash(&key)
        && !keys.contains(&dotted_key)
    {
        keys.push(dotted_key);
    }

    keys
}

fn dot_last_numeric_dash(key: &str) -> Option<String> {
    let (prefix, last_segment) = key.rsplit_once('-')?;
    let (_, previous_segment) = prefix.rsplit_once('-')?;
    if !last_segment.chars().all(|c| c.is_ascii_digit())
        || !previous_segment.chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    Some(format!("{prefix}.{last_segment}"))
}

fn copilot_headers(
    device_id: &str,
    machine_id: &str,
    session_id: &str,
    initiator: Option<&str>,
    is_vision: bool,
) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert("editor-version", HeaderValue::from_static("vscode/1.114.0"));
    h.insert(
        "editor-plugin-version",
        HeaderValue::from_static("copilot-chat/0.26.7"),
    );
    h.insert(
        "user-agent",
        HeaderValue::from_static("GitHubCopilotChat/0.26.7"),
    );
    h.insert(
        "x-github-api-version",
        HeaderValue::from_static("2026-01-09"),
    );
    h.insert(
        "copilot-integration-id",
        HeaderValue::from_static("vscode-chat"),
    );
    h.insert(
        "openai-intent",
        HeaderValue::from_static("conversation-agent"),
    );
    if let Ok(val) = HeaderValue::from_str(device_id) {
        h.insert("editor-device-id", val);
    }

    // Per-request unique ID, matching the real extension behavior
    if let Ok(val) = HeaderValue::from_str(&uuid::Uuid::new_v4().to_string()) {
        h.insert("x-request-id", val);
    }

    // Session context headers sent by the real VSCode Copilot extension.
    // These are used by the API for rate-limit bucketing and telemetry.
    if let Ok(val) = HeaderValue::from_str(machine_id) {
        h.insert("vscode-machineid", val);
    }
    if let Ok(val) = HeaderValue::from_str(session_id) {
        h.insert("vscode-sessionid", val);
    }

    // X-Initiator: "user" consumes premium, "agent" does not
    h.insert(
        "X-Initiator",
        HeaderValue::from_static(if initiator == Some("agent") {
            "agent"
        } else {
            "user"
        }),
    );

    if is_vision {
        h.insert("Copilot-Vision-Request", HeaderValue::from_static("true"));
    }

    h
}

/// Forward upstream response to client
pub async fn forward_response(resp: reqwest::Response) -> Result<Response, Error> {
    let status = resp.status();
    let headers = resp.headers().clone();

    let is_stream = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("text/event-stream"))
        .unwrap_or(false);

    let mut builder = Response::builder().status(status);

    for (key, value) in headers.iter() {
        if !HOP_BY_HOP.contains(&key.as_str()) {
            builder = builder.header(key, value);
        }
    }

    if is_stream && !headers.contains_key("cache-control") {
        builder = builder.header("Cache-Control", "no-cache");
    }

    let body = if is_stream {
        let stream = resp.bytes_stream().map_err(std::io::Error::other);
        Body::from_stream(stream)
    } else {
        Body::from(resp.bytes().await?)
    };

    builder
        .body(body)
        .map_err(|e| Error::InvalidRequest(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_lookup_keys_include_dot_and_dash_spellings() {
        assert_eq!(
            model_lookup_keys("claude-opus-4.7"),
            vec!["claude-opus-4.7".to_string(), "claude-opus-4-7".to_string(),]
        );
        assert_eq!(
            model_lookup_keys("claude-opus-4-7"),
            vec!["claude-opus-4-7".to_string(), "claude-opus-4.7".to_string(),]
        );
    }

    #[test]
    fn fresh_unknown_model_cache_miss_is_cached_briefly() {
        let cache = ModelsCache {
            fetched_at: Instant::now(),
            reasoning_efforts_by_model: HashMap::new(),
            model_ids: HashSet::new(),
        };
        let keys = vec!["brand-new-model".to_string()];

        assert_eq!(
            cached_reasoning_efforts_from_cache(&cache, &keys),
            Some(None)
        );
    }

    #[test]
    fn older_unknown_model_cache_miss_triggers_refresh() {
        let cache = ModelsCache {
            fetched_at: Instant::now() - MODELS_UNKNOWN_REFRESH_TTL - Duration::from_secs(1),
            reasoning_efforts_by_model: HashMap::new(),
            model_ids: HashSet::new(),
        };
        let keys = vec!["brand-new-model".to_string()];

        assert_eq!(cached_reasoning_efforts_from_cache(&cache, &keys), None);
    }
}
