//! Shared OpenAI-compatible handlers reused by `/v1/*`, Amp, and Droid routes.

use crate::claude::{
    analyze_claude_request, error_from_proxy, is_native_claude_model, merge_tool_result_blocks,
    validate_anthropic_headers,
};
use crate::error::Error;
use crate::initiator::{
    RequestAnalysis, analyze_openai_chat_completions, analyze_openai_responses,
};
use crate::proxy::forward_response;
use crate::server::AppState;
use axum::body::Bytes;
use axum::http::{HeaderMap, Method};
use axum::response::Response;

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
    let query = query.map(|q| format!("?{q}")).unwrap_or_default();
    let analysis = analyze_openai_request(api_path, &method, &body, headers);

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
    forward_response(resp).await
}

pub async fn handle_native_claude_passthrough(
    state: &AppState,
    method: Method,
    api_path: &str,
    query: Option<&str>,
    headers: &HeaderMap,
    body: Bytes,
    validate_client_api_key: bool,
) -> Result<Response, Error> {
    if method != Method::POST {
        return Ok(error_from_proxy(Error::InvalidRequest(format!(
            "Only POST is supported for {api_path}"
        ))));
    }
    if validate_client_api_key && let Some(response) = validate_anthropic_headers(headers) {
        return Ok(response);
    }

    let metadata = match analyze_claude_request(&body, Some(headers)) {
        Ok(metadata) => metadata,
        Err(error) => return Ok(error_from_proxy(error)),
    };
    if !is_native_claude_model(&metadata.model) {
        return Ok(error_from_proxy(Error::InvalidRequest(
            "Only native Claude models are supported on Anthropic routes; use an OpenAI-compatible route for other models".to_string(),
        )));
    }

    let body = merge_tool_result_blocks(&body).unwrap_or(body);
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
    forward_response(response).await
}

#[cfg(test)]
mod tests {
    use super::analyze_openai_request;
    use axum::http::{HeaderMap, Method};

    #[test]
    fn claude_model_uses_normal_openai_analysis() {
        let body = br#"{
            "model": "claude-sonnet-4.6",
            "messages": [{"role": "user", "content": "Hello"}]
        }"#;
        let analysis =
            analyze_openai_request("chat/completions", &Method::POST, body, &HeaderMap::new())
                .unwrap();

        assert_eq!(analysis.initiator, "user");
        assert!(!analysis.is_vision);
    }
}
