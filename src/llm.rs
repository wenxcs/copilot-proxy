//! OpenAI-compatible and native Claude handlers for `/v1/*` routes.

use crate::claude::{
    analyze_claude_request, error_from_proxy, is_native_claude_model, merge_tool_result_blocks,
    normalize_native_claude_body,
};
use crate::error::Error;
use crate::initiator::{
    RequestAnalysis, analyze_openai_chat_completions, analyze_openai_responses,
};
use crate::model_policy::{ModelPolicy, UsagePermit};
use crate::proxy::forward_response;
use crate::server::AppState;
use axum::body::Bytes;
use axum::http::{HeaderMap, Method};
use axum::response::Response;
use serde_json::Value;
use std::sync::Arc;

/// Policies inspect model identifiers only when configured; bytes remain unchanged
/// except when metered chat streams need upstream usage reporting.
fn prepare_policy_request(
    policy: &Arc<ModelPolicy>,
    method: &Method,
    path: &str,
    body: Bytes,
) -> Result<(Bytes, Option<UsagePermit>), Error> {
    if !policy.is_enabled() || !matches!(*method, Method::POST | Method::PUT | Method::PATCH) {
        return Ok((body, None));
    }
    let requires_model = matches!(
        path,
        "chat/completions"
            | "responses"
            | "completions"
            | "embeddings"
            | "messages"
            | "messages/count_tokens"
    );
    let mut value: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) if !requires_model => return Ok((body, None)),
        Err(_) => {
            return Err(Error::InvalidRequest(
                "Model policy requires a JSON request body".to_string(),
            ));
        }
    };
    let model = match value.get("model") {
        Some(Value::String(model)) if !model.is_empty() && model.trim() == model => model.clone(),
        None if !requires_model => return Ok((body, None)),
        _ => return Err(Error::InvalidRequest("Model policy requires a nonempty string model identifier without surrounding whitespace".to_string())),
    };
    let permit = policy.begin(&model, path != "messages/count_tokens")?;
    if let Some(permit) = permit {
        if path == "responses" && value.get("background").and_then(Value::as_bool) == Some(true) {
            permit.finish(Some(0))?;
            return Err(Error::InvalidRequest(
                "Background responses are not supported for models with token limits".to_string(),
            ));
        }
        if path == "chat/completions" && value.get("stream").and_then(Value::as_bool) == Some(true)
        {
            let options = value
                .as_object_mut()
                .expect("model field implies object")
                .entry("stream_options")
                .or_insert_with(|| serde_json::json!({}));
            if options.is_null() {
                *options = serde_json::json!({});
            }
            let Some(options) = options.as_object_mut() else {
                permit.finish(Some(0))?;
                return Err(Error::InvalidRequest(
                    "stream_options must be an object for metered streaming requests".to_string(),
                ));
            };
            options.insert("include_usage".to_string(), Value::Bool(true));
            let bytes = serde_json::to_vec(&value)
                .map_err(|error| Error::InvalidRequest(error.to_string()))?;
            return Ok((Bytes::from(bytes), Some(permit)));
        }
        return Ok((body, Some(permit)));
    }
    Ok((body, None))
}

async fn forward_policy_response(
    response: reqwest::Response,
    permit: Option<UsagePermit>,
    native: bool,
) -> Result<Response, Error> {
    match permit {
        Some(permit) => crate::metered_response::forward_response(response, permit, native).await,
        None => forward_response(response).await,
    }
}

fn analyze_openai_request(path: &str, method: &Method, body: &[u8]) -> Option<RequestAnalysis> {
    if *method != Method::POST {
        return None;
    }
    match path {
        "chat/completions" => Some(analyze_openai_chat_completions(body)),
        "responses" => Some(analyze_openai_responses(body)),
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
    let (body, permit) = prepare_policy_request(&state.policy, &method, api_path, body)?;
    let content_type = headers.get("content-type").and_then(|v| v.to_str().ok());
    let query = query.map(|q| format!("?{q}")).unwrap_or_default();
    let analysis = analyze_openai_request(api_path, &method, &body);

    let resp = state
        .proxy
        .forward(
            &format!("/{api_path}{query}"),
            method,
            body,
            content_type,
            analysis.map(|a| a.initiator),
            analysis.map(|a| a.is_vision).unwrap_or(false),
        )
        .await?;
    forward_policy_response(resp, permit, false).await
}

pub async fn handle_native_claude_passthrough(
    state: &AppState,
    method: Method,
    api_path: &str,
    query: Option<&str>,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<Response, Error> {
    if method != Method::POST {
        return Ok(error_from_proxy(Error::InvalidRequest(format!(
            "Only POST is supported for {api_path}"
        ))));
    }
    let metadata = match analyze_claude_request(&body) {
        Ok(metadata) => metadata,
        Err(error) => return Ok(error_from_proxy(error)),
    };
    if !is_native_claude_model(&metadata.model) {
        return Ok(error_from_proxy(Error::InvalidRequest(
            "Only native Claude models are supported on Anthropic routes; use an OpenAI-compatible route for other models".to_string(),
        )));
    }

    let body = merge_tool_result_blocks(&body).unwrap_or(body);
    let body = normalize_native_claude_body(body);
    let (body, permit) = match prepare_policy_request(&state.policy, &method, api_path, body) {
        Ok(request) => request,
        Err(error) => return Ok(error_from_proxy(error)),
    };
    let content_type = headers
        .get("content-type")
        .and_then(|value| value.to_str().ok());
    let query = query.map(|value| format!("?{value}")).unwrap_or_default();
    let response = match state
        .proxy
        .forward(
            &format!("/v1/{api_path}{query}"),
            method,
            body,
            content_type,
            Some(metadata.initiator),
            metadata.is_vision,
        )
        .await
    {
        Ok(response) => response,
        Err(error) => return Ok(error_from_proxy(error)),
    };
    match forward_policy_response(response, permit, true).await {
        Ok(response) => Ok(response),
        Err(error) => Ok(error_from_proxy(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::{analyze_openai_request, prepare_policy_request};
    use crate::{error::Error, model_policy::ModelPolicy};
    use axum::body::Bytes;
    use axum::http::Method;

    #[test]
    fn claude_model_uses_normal_openai_analysis() {
        let body = br#"{
            "model": "claude-sonnet-4.6",
            "messages": [{"role": "user", "content": "Hello"}]
        }"#;
        let analysis = analyze_openai_request("chat/completions", &Method::POST, body).unwrap();

        assert_eq!(analysis.initiator, "user");
        assert!(!analysis.is_vision);
    }

    #[test]
    fn unconfigured_policy_preserves_even_invalid_json_and_configured_opaque_routes() {
        let body = Bytes::from_static(b"\0opaque\xff");
        let disabled = ModelPolicy::new("", None).unwrap();
        let (forwarded, permit) =
            prepare_policy_request(&disabled, &Method::POST, "chat/completions", body.clone())
                .unwrap();
        assert_eq!(forwarded, body);
        assert!(permit.is_none());
        let restricted = ModelPolicy::new("-all", None).unwrap();
        let (forwarded, permit) =
            prepare_policy_request(&restricted, &Method::POST, "files", body.clone()).unwrap();
        assert_eq!(forwarded, body);
        assert!(permit.is_none());
        assert!(matches!(
            prepare_policy_request(&restricted, &Method::POST, "responses", body),
            Err(Error::InvalidRequest(_))
        ));
    }

    #[test]
    fn rejects_unverifiable_background_or_invalid_stream_options_without_spending_budget() {
        let policy = ModelPolicy::new("5h:model:10", None).unwrap();
        for (path, body) in [
            (
                "responses",
                Bytes::from_static(br#"{"model":"model","background":true}"#),
            ),
            (
                "chat/completions",
                Bytes::from_static(br#"{"model":"model","stream":true,"stream_options":1}"#),
            ),
        ] {
            assert!(matches!(
                prepare_policy_request(&policy, &Method::POST, path, body),
                Err(Error::InvalidRequest(_))
            ));
            let snapshot = policy.snapshot().unwrap();
            assert_eq!(snapshot["models"]["model"]["5h"]["used"], 0);
            assert_eq!(snapshot["models"]["model"]["in_flight"], false);
        }
        let unmetered = ModelPolicy::new("+model", None).unwrap();
        let body = Bytes::from_static(br#"{ "model": "model", "stream": true }"#);
        let (forwarded, permit) =
            prepare_policy_request(&unmetered, &Method::POST, "chat/completions", body.clone())
                .unwrap();
        assert_eq!(forwarded, body);
        assert!(permit.is_none());
    }
}
