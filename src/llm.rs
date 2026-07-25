//! Shared LLM surface handlers reused by `/v1/*`, Amp, and Droid routes.

use crate::claude::{
    analyze_claude_request, convert_claude_request, convert_openai_response, error_from_proxy,
    extract_anthropic_model, is_native_claude_model, merge_tool_result_blocks,
    validate_anthropic_headers,
};
use crate::error::Error;
use crate::gemini::{convert_gemini_request, convert_openai_to_gemini_response};
use crate::initiator::{
    RequestAnalysis, analyze_openai_chat_completions, analyze_openai_responses,
};
use crate::proxy::forward_response;
use crate::server::AppState;
use crate::token_counter::handle_count_tokens;
use axum::body::Bytes;
use axum::http::{HeaderMap, Method};
use axum::response::Response;
use serde_json::Value;

fn analyze_openai_request(
    path: &str,
    method: &Method,
    body: &[u8],
    headers: &HeaderMap,
) -> Option<RequestAnalysis> {
    if *method != Method::POST {
        return None;
    }
    match path {
        "chat/completions" => Some(analyze_openai_chat_completions(body, Some(headers))),
        "responses" => Some(analyze_openai_responses(body, Some(headers))),
        _ => None,
    }
}

pub async fn handle_openai_passthrough(
    state: &AppState,
    method: Method,
    api_path: &str,
    query: Option<&str>,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<Response, Error> {
    let content_type = headers.get("content-type").and_then(|v| v.to_str().ok());
    let query = query.map(|q| format!("?{}", q)).unwrap_or_default();
    let analysis = analyze_openai_request(api_path, &method, &body, headers);

    let resp = state
        .proxy
        .forward(
            &format!("/{}{}", api_path, query),
            method,
            body,
            content_type,
            analysis.map(|a| a.initiator),
            analysis.map(|a| a.is_vision).unwrap_or(false),
        )
        .await?;
    forward_response(resp).await
}

/// Model aliases: (substring_to_replace, replacement).
/// Applied in order; first match wins.
/// Add new entries here when Copilot removes or renames a model.
const ANTHROPIC_MODEL_ALIASES: &[(&str, &str)] = &[
    ("claude-opus-4.6", "claude-opus-4.7"),
    ("claude-opus-4-6", "claude-opus-4-7"),
];

/// Fallback upstream models used when a client requests a native Claude model
/// name (e.g. the dated Anthropic public IDs like `claude-sonnet-4-20250514`)
/// that Copilot upstream does not expose. Overridable via the `SMALL_MODEL`,
/// `MIDDLE_MODEL`, and `BIG_MODEL` environment variables.
const DEFAULT_SMALL_MODEL: &str = "claude-haiku-4.5";
const DEFAULT_MIDDLE_MODEL: &str = "claude-sonnet-5";
const DEFAULT_BIG_MODEL: &str = "claude-opus-5";

/// Pick the configured fallback upstream model for a native Claude request,
/// based on its family (haiku/sonnet/opus). Bare `claude-*` names without a
/// recognizable family fall back to the middle (sonnet) tier.
fn family_default_model(model: &str) -> Option<String> {
    let lower = model.to_lowercase();
    let target = if lower.contains("haiku") {
        std::env::var("SMALL_MODEL").unwrap_or_else(|_| DEFAULT_SMALL_MODEL.to_string())
    } else if lower.contains("opus") {
        std::env::var("BIG_MODEL").unwrap_or_else(|_| DEFAULT_BIG_MODEL.to_string())
    } else if lower.contains("sonnet") {
        std::env::var("MIDDLE_MODEL").unwrap_or_else(|_| DEFAULT_MIDDLE_MODEL.to_string())
    } else if lower.contains("claude") {
        std::env::var("MIDDLE_MODEL").unwrap_or_else(|_| DEFAULT_MIDDLE_MODEL.to_string())
    } else {
        return None;
    };
    Some(target)
}

/// Convert `budget_tokens` to a Copilot `output_config.effort` string.
fn budget_tokens_to_effort(budget_tokens: u64) -> &'static str {
    if budget_tokens < 4_000 {
        "low"
    } else if budget_tokens < 16_000 {
        "medium"
    } else if budget_tokens < 28_000 {
        "high"
    } else {
        "xhigh"
    }
}

fn effort_rank(effort: &str) -> Option<usize> {
    match effort {
        "none" => Some(0),
        "low" => Some(1),
        "medium" => Some(2),
        "high" => Some(3),
        "xhigh" => Some(4),
        _ => None,
    }
}

fn choose_supported_effort(desired: &str, supported: Option<&[String]>) -> String {
    let Some(supported) = supported.filter(|values| !values.is_empty()) else {
        return desired.to_string();
    };
    if supported.iter().any(|value| value == desired) {
        return desired.to_string();
    }

    let desired_rank = effort_rank(desired);
    if let Some(desired_rank) = desired_rank
        && let Some((effort, _)) = supported
            .iter()
            .filter_map(|effort| effort_rank(effort).map(|rank| (effort, rank)))
            .filter(|(_, rank)| *rank <= desired_rank)
            .max_by_key(|(_, rank)| *rank)
    {
        return effort.clone();
    }

    if let Some((effort, _)) = supported
        .iter()
        .filter_map(|effort| effort_rank(effort).map(|rank| (effort, rank)))
        .min_by_key(|(_, rank)| rank.abs_diff(desired_rank.unwrap_or(0)))
    {
        return effort.clone();
    }

    supported[0].clone()
}

/// Rewrite an Anthropic request body:
/// 1. Remap deprecated model aliases.
/// 2. Convert `thinking.type: "enabled"` + `budget_tokens` to
///    `thinking.type: "adaptive"` + `output_config.effort`, which is what
///    Copilot's adaptive-thinking models (e.g. claude-opus-4.7) require.
fn remap_anthropic_model(body: Bytes, supported_reasoning_efforts: Option<&[String]>) -> Bytes {
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return body;
    };
    let obj = match value.as_object_mut() {
        Some(o) => o,
        None => return body,
    };

    // 1. Model alias remapping.
    let mut model_changed = false;
    if let Some(model) = obj.get("model").and_then(|v| v.as_str()) {
        let mut remapped = model.to_string();
        for (from, to) in ANTHROPIC_MODEL_ALIASES {
            if remapped.contains(from) {
                remapped = remapped.replace(from, to);
                break;
            }
        }
        if remapped != model {
            tracing::debug!(
                target: "llm",
                old_model = model,
                new_model = %remapped,
                "remapping deprecated Anthropic model"
            );
            obj.insert("model".to_string(), serde_json::Value::String(remapped));
            model_changed = true;
        }
    }

    // 2. Thinking type conversion: "enabled" -> "adaptive" + output_config.effort.
    let mut thinking_changed = false;
    if let Some(thinking) = obj.get_mut("thinking").and_then(|v| v.as_object_mut()) {
        if thinking.get("type").and_then(|v| v.as_str()) == Some("enabled") {
            let budget = thinking
                .get("budget_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(10_000);
            thinking.insert(
                "type".to_string(),
                serde_json::Value::String("adaptive".to_string()),
            );
            thinking.remove("budget_tokens");
            let desired_effort = budget_tokens_to_effort(budget);
            let effort = choose_supported_effort(desired_effort, supported_reasoning_efforts);
            tracing::debug!(
                target: "llm",
                budget_tokens = budget,
                desired_effort,
                effort = %effort,
                supported_reasoning_efforts = ?supported_reasoning_efforts,
                "converting thinking.type enabled -> adaptive"
            );
            thinking_changed = true;
            // Insert output_config.effort (merge with existing if present).
            let effort_val = serde_json::json!({ "effort": effort.clone() });
            obj.entry("output_config".to_string())
                .and_modify(|v| {
                    if let Some(cfg) = v.as_object_mut() {
                        cfg.insert(
                            "effort".to_string(),
                            serde_json::Value::String(effort.clone()),
                        );
                    }
                })
                .or_insert(effort_val);
        }
    }

    // 3. Clamp an existing output_config.effort too. Some clients already send
    // adaptive thinking in Copilot's native shape.
    let mut output_config_changed = false;
    if let Some(output_config) = obj.get_mut("output_config").and_then(|v| v.as_object_mut())
        && let Some(current_effort) = output_config.get("effort").and_then(|v| v.as_str())
    {
        let clamped_effort = choose_supported_effort(current_effort, supported_reasoning_efforts);
        if clamped_effort != current_effort {
            tracing::debug!(
                target: "llm",
                old_effort = current_effort,
                new_effort = %clamped_effort,
                supported_reasoning_efforts = ?supported_reasoning_efforts,
                "clamping output_config.effort to supported model value"
            );
            output_config.insert(
                "effort".to_string(),
                serde_json::Value::String(clamped_effort),
            );
            output_config_changed = true;
        }
    }

    if model_changed || thinking_changed || output_config_changed {
        if let Ok(bytes) = serde_json::to_vec(&value) {
            return Bytes::from(bytes);
        }
    }
    body
}

fn remapped_anthropic_model_name(body: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let model = value.get("model").and_then(|v| v.as_str())?;
    let mut remapped = model.to_string();
    for (from, to) in ANTHROPIC_MODEL_ALIASES {
        if remapped.contains(from) {
            remapped = remapped.replace(from, to);
            break;
        }
    }
    Some(remapped)
}

fn needs_supported_reasoning_effort_lookup(body: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            let has_enabled_thinking = value
                .get("thinking")
                .and_then(|thinking| thinking.get("type"))
                .and_then(|value| value.as_str())
                .map(|kind| kind == "enabled")
                .unwrap_or(false);
            let has_output_effort = value
                .get("output_config")
                .and_then(|config| config.get("effort"))
                .and_then(|value| value.as_str())
                .is_some();
            Some(has_enabled_thinking || has_output_effort)
        })
        .unwrap_or(false)
}

async fn remap_anthropic_model_for_copilot(state: &AppState, body: Bytes) -> Bytes {
    let body = normalize_unsupported_native_claude_model(state, body).await;
    let supported_reasoning_efforts = if needs_supported_reasoning_effort_lookup(&body) {
        if let Some(model) = remapped_anthropic_model_name(&body) {
            match state.proxy.supported_reasoning_efforts(&model).await {
                Ok(efforts) => efforts,
                Err(err) => {
                    tracing::warn!(
                        target: "llm",
                        model = %model,
                        error = %err,
                        "failed to fetch Copilot model reasoning efforts; using budget-derived effort"
                    );
                    None
                }
            }
        } else {
            None
        }
    } else {
        None
    };
    remap_anthropic_model(body, supported_reasoning_efforts.as_deref())
}

/// Rewrite the `model` field when a client requests a native Claude model that
/// Copilot upstream does not expose (e.g. the dated Anthropic public IDs such
/// as `claude-sonnet-4-20250514`). The request is remapped to the configured
/// fallback model for its family so it succeeds instead of returning
/// `model_not_supported`. Requests that already name a supported upstream model
/// are left untouched.
async fn normalize_unsupported_native_claude_model(state: &AppState, body: Bytes) -> Bytes {
    let Ok(mut value) = serde_json::from_slice::<Value>(&body) else {
        return body;
    };
    let model = match value.get("model").and_then(|v| v.as_str()) {
        Some(model) => model.to_string(),
        None => return body,
    };
    if !is_native_claude_model(&model) {
        return body;
    }
    // Only remap when the model list was fetched and does NOT contain the
    // requested model. Unknown (fetch failed) leaves the request untouched.
    if state.proxy.is_supported_model(&model).await != Some(false) {
        return body;
    }
    let Some(target) = family_default_model(&model) else {
        return body;
    };
    if target == model {
        return body;
    }
    tracing::debug!(
        target: "llm",
        requested_model = %model,
        mapped_model = %target,
        "remapping unsupported native Claude model to a supported upstream model"
    );
    let Some(obj) = value.as_object_mut() else {
        return body;
    };
    obj.insert("model".to_string(), Value::String(target));
    match serde_json::to_vec(&value) {
        Ok(bytes) => Bytes::from(bytes),
        Err(_) => body,
    }
}

pub async fn handle_anthropic_compat(
    state: &AppState,
    method: Method,
    api_path: &str,
    query: Option<&str>,
    headers: &HeaderMap,
    body: Bytes,
    validate_client_api_key: bool,
) -> Result<Response, Error> {
    let query = query.map(|q| format!("?{}", q)).unwrap_or_default();
    let content_type = headers.get("content-type").and_then(|v| v.to_str().ok());

    match api_path {
        "messages/count_tokens" => {
            if method != Method::POST {
                return Ok(error_from_proxy(Error::InvalidRequest(
                    "Only POST is supported for messages/count_tokens".to_string(),
                )));
            }
            let body = remap_anthropic_model_for_copilot(state, body).await;
            if let Some(model) = extract_anthropic_model(&body)
                && is_native_claude_model(&model)
            {
                let resp = match state
                    .proxy
                    .forward(
                        &format!("/v1/messages/count_tokens{query}"),
                        method,
                        body,
                        content_type,
                        None,
                        false,
                    )
                    .await
                {
                    Ok(resp) => resp,
                    Err(err) => return Ok(error_from_proxy(err)),
                };
                return forward_response(resp).await;
            }
            handle_count_tokens(body).await
        }
        "messages" => {
            if method != Method::POST {
                return Ok(error_from_proxy(Error::InvalidRequest(
                    "Only POST is supported for messages".to_string(),
                )));
            }

            if validate_client_api_key && let Some(resp) = validate_anthropic_headers(headers) {
                return Ok(resp);
            }

            let body = remap_anthropic_model_for_copilot(state, body).await;
            let metadata = match analyze_claude_request(&body, Some(headers)) {
                Ok(metadata) => metadata,
                Err(err) => return Ok(error_from_proxy(err)),
            };
            if is_native_claude_model(&metadata.model) {
                let forwarded_body = merge_tool_result_blocks(&body)
                    .map(Bytes::from)
                    .unwrap_or(body);
                let resp = match state
                    .proxy
                    .forward(
                        &format!("/v1/messages{}", query),
                        method,
                        forwarded_body,
                        content_type,
                        Some(&metadata.initiator),
                        metadata.is_vision,
                    )
                    .await
                {
                    Ok(resp) => resp,
                    Err(err) => return Ok(error_from_proxy(err)),
                };
                return forward_response(resp).await;
            }

            let converted = match convert_claude_request(body, Some(headers)) {
                Ok(converted) => converted,
                Err(err) => return Ok(error_from_proxy(err)),
            };
            let resp = match state
                .proxy
                .forward(
                    &format!("/chat/completions{}", query),
                    method,
                    converted.body,
                    Some("application/json"),
                    Some(&converted.initiator),
                    converted.is_vision,
                )
                .await
            {
                Ok(resp) => resp,
                Err(err) => return Ok(error_from_proxy(err)),
            };
            match convert_openai_response(resp, converted.model, converted.stream).await {
                Ok(response) => Ok(response),
                Err(err) => Ok(error_from_proxy(err)),
            }
        }
        _ => {
            handle_openai_passthrough(
                state,
                method,
                api_path,
                Some(query.trim_start_matches('?')),
                headers,
                body,
            )
            .await
        }
    }
}

pub async fn handle_gemini_generate_content(
    state: &AppState,
    method: Method,
    model: &str,
    query: Option<&str>,
    body: Bytes,
    stream: bool,
) -> Result<Response, Error> {
    let query = query.map(|q| format!("?{}", q)).unwrap_or_default();
    let converted = match convert_gemini_request(model, body, stream) {
        Ok(c) => c,
        Err(e) => return Ok(error_from_proxy(e)),
    };
    let resp = match state
        .proxy
        .forward(
            &format!("/chat/completions{}", query),
            method,
            converted.body,
            Some("application/json"),
            Some(converted.initiator),
            converted.is_vision,
        )
        .await
    {
        Ok(r) => r,
        Err(e) => return Ok(error_from_proxy(e)),
    };
    match convert_openai_to_gemini_response(resp, converted.model, converted.stream).await {
        Ok(r) => Ok(r),
        Err(e) => Ok(error_from_proxy(e)),
    }
}

pub fn extract_model_field(body: &[u8]) -> Result<String, Error> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|e| Error::InvalidRequest(format!("Invalid JSON body: {e}")))?;
    value
        .get("model")
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
        .ok_or_else(|| Error::InvalidRequest("Missing required field: model".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remapped_value(
        value: serde_json::Value,
        supported_reasoning_efforts: Option<&[String]>,
    ) -> serde_json::Value {
        let body = serde_json::to_vec(&value).unwrap();
        let remapped = remap_anthropic_model(Bytes::from(body), supported_reasoning_efforts);
        serde_json::from_slice(&remapped).unwrap()
    }

    #[test]
    fn thinking_effort_is_clamped_to_copilot_supported_values() {
        let supported = vec!["medium".to_string()];
        let value = remapped_value(
            serde_json::json!({
                "model": "claude-opus-4.7",
                "max_tokens": 1024,
                "thinking": {
                    "type": "enabled",
                    "budget_tokens": 20_000
                },
                "messages": []
            }),
            Some(&supported),
        );

        assert_eq!(value["thinking"]["type"], "adaptive");
        assert!(value["thinking"].get("budget_tokens").is_none());
        assert_eq!(value["output_config"]["effort"], "medium");
    }

    #[test]
    fn alias_uses_remapped_model_supported_effort_limit() {
        let supported = vec!["medium".to_string()];
        let value = remapped_value(
            serde_json::json!({
                "model": "claude-opus-4.6",
                "max_tokens": 1024,
                "thinking": {
                    "type": "enabled",
                    "budget_tokens": 40_000
                },
                "messages": []
            }),
            Some(&supported),
        );

        assert_eq!(value["model"], "claude-opus-4.7");
        assert_eq!(value["output_config"]["effort"], "medium");
    }

    #[test]
    fn non_opus_models_keep_budget_based_effort() {
        let value = remapped_value(
            serde_json::json!({
                "model": "claude-sonnet-4.5",
                "max_tokens": 1024,
                "thinking": {
                    "type": "enabled",
                    "budget_tokens": 20_000
                },
                "messages": []
            }),
            None,
        );

        assert_eq!(value["output_config"]["effort"], "high");
    }

    #[test]
    fn unsupported_xhigh_effort_falls_back_to_high_when_available() {
        let supported = vec!["low".to_string(), "medium".to_string(), "high".to_string()];
        assert_eq!(
            choose_supported_effort("xhigh", Some(&supported)),
            "high".to_string()
        );
    }

    #[test]
    fn existing_output_config_effort_is_clamped() {
        let supported = vec!["medium".to_string()];
        let value = remapped_value(
            serde_json::json!({
                "model": "claude-opus-4.7",
                "max_tokens": 1024,
                "thinking": {
                    "type": "adaptive"
                },
                "output_config": {
                    "effort": "high"
                },
                "messages": []
            }),
            Some(&supported),
        );

        assert_eq!(value["thinking"]["type"], "adaptive");
        assert_eq!(value["output_config"]["effort"], "medium");
    }

    #[test]
    fn output_config_effort_triggers_supported_effort_lookup() {
        let value = serde_json::json!({
            "model": "claude-opus-4.7",
            "output_config": {
                "effort": "high"
            },
            "messages": []
        });
        let body = serde_json::to_vec(&value).unwrap();

        assert!(needs_supported_reasoning_effort_lookup(&body));
    }

    #[test]
    fn family_default_model_maps_dated_anthropic_names_by_family() {
        assert_eq!(
            family_default_model("claude-3-5-haiku-20241022").as_deref(),
            Some("claude-haiku-4.5")
        );
        assert_eq!(
            family_default_model("claude-sonnet-4-20250514").as_deref(),
            Some("claude-sonnet-5")
        );
        assert_eq!(
            family_default_model("claude-opus-4-20250514").as_deref(),
            Some("claude-opus-5")
        );
    }

    #[test]
    fn family_default_model_falls_back_to_middle_for_bare_claude_names() {
        assert_eq!(
            family_default_model("claude-2.1").as_deref(),
            Some("claude-sonnet-5")
        );
    }

    #[test]
    fn family_default_model_ignores_non_claude_models() {
        assert_eq!(family_default_model("gpt-4o"), None);
    }
}
