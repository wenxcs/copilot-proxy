//! Shared OpenAI-compatible handlers reused by `/v1/*`, Amp, and Droid routes.

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
