//! Native Claude/Anthropic request analysis for direct Copilot passthrough.

use crate::error::Error;
use crate::initiator::infer_initiator_claude;
use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use serde_json::Value;

const CONTENT_IMAGE: &str = "image";
const CONTENT_TEXT: &str = "text";
const CONTENT_TOOL_RESULT: &str = "tool_result";

pub struct ClaudeRequestMetadata {
    pub model: String,
    pub initiator: &'static str,
    pub is_vision: bool,
}

pub fn analyze_claude_request(
    body: &[u8],
    headers: Option<&HeaderMap>,
) -> Result<ClaudeRequestMetadata, Error> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|e| Error::InvalidRequest(format!("Invalid JSON body: {e}")))?;
    let model = value
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::InvalidRequest("Missing required field: model".to_string()))?
        .to_string();
    let messages = value
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::InvalidRequest("Missing required field: messages".to_string()))?;
    let initiator = infer_initiator_claude(messages, headers);
    let is_vision = messages.iter().any(|message| {
        message
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|parts| {
                parts
                    .iter()
                    .any(|part| part.get("type").and_then(Value::as_str) == Some(CONTENT_IMAGE))
            })
    });

    Ok(ClaudeRequestMetadata {
        model,
        initiator,
        is_vision,
    })
}

pub fn is_native_claude_model(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model.contains("claude")
        || model.contains("sonnet")
        || model.contains("haiku")
        || model.contains("opus")
}

pub fn validate_anthropic_headers(headers: &HeaderMap) -> Option<Response> {
    let expected = std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())?;

    if extract_client_api_key(headers).as_deref() == Some(expected.as_str()) {
        return None;
    }

    Some(error_response(
        StatusCode::UNAUTHORIZED,
        "authentication_error",
        "Invalid API key. Please provide a valid Anthropic API key.",
    ))
}

pub fn error_from_proxy(error: Error) -> Response {
    let (status, error_type) = match &error {
        Error::Auth(_) => (StatusCode::UNAUTHORIZED, "authentication_error"),
        Error::InvalidRequest(_) => (StatusCode::BAD_REQUEST, "invalid_request_error"),
        Error::Upstream(_) => (StatusCode::BAD_GATEWAY, "api_error"),
        Error::Config(_) | Error::Io(_) => (StatusCode::INTERNAL_SERVER_ERROR, "api_error"),
    };
    error_response(status, error_type, &error.to_string())
}

pub fn merge_tool_result_blocks(body: &[u8]) -> Option<Bytes> {
    let mut value: Value = serde_json::from_slice(body).ok()?;
    let messages = value.get_mut("messages")?.as_array_mut()?;
    let mut modified = false;

    for message in messages {
        if message.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        let has_tool_result = blocks
            .iter()
            .any(|block| block.get("type").and_then(Value::as_str) == Some(CONTENT_TOOL_RESULT));
        let has_text = blocks
            .iter()
            .any(|block| block.get("type").and_then(Value::as_str) == Some(CONTENT_TEXT));
        if !has_tool_result || !has_text {
            continue;
        }

        let old_blocks = std::mem::take(blocks);
        let mut last_tool_result_index = None;
        for block in old_blocks {
            match block.get("type").and_then(Value::as_str) {
                Some(CONTENT_TOOL_RESULT) => {
                    blocks.push(block);
                    last_tool_result_index = Some(blocks.len() - 1);
                }
                Some(CONTENT_TEXT) if last_tool_result_index.is_some() => {
                    let tool_result = &mut blocks[last_tool_result_index.unwrap()];
                    match tool_result.get_mut("content") {
                        Some(content) if content.is_array() => {
                            content.as_array_mut().unwrap().push(block);
                        }
                        Some(content) if content.is_string() => {
                            let existing = content.as_str().unwrap().to_string();
                            *content = serde_json::json!([
                                {"type": CONTENT_TEXT, "text": existing},
                                block
                            ]);
                        }
                        _ => tool_result["content"] = serde_json::json!([block]),
                    }
                    modified = true;
                }
                _ => blocks.push(block),
            }
        }
    }

    modified
        .then(|| serde_json::to_vec(&value).ok().map(Bytes::from))
        .flatten()
}

fn extract_client_api_key(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
    {
        let value = value.trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .and_then(|value| {
            value
                .strip_prefix("Bearer ")
                .or_else(|| value.strip_prefix("bearer "))
        })
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn error_response(status: StatusCode, error_type: &str, message: &str) -> Response {
    let body = serde_json::json!({
        "type": "error",
        "error": {
            "type": error_type,
            "message": message
        }
    });
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .expect("valid Anthropic error response")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_only_claude_families_as_native() {
        assert!(is_native_claude_model("claude-sonnet-4.6"));
        assert!(is_native_claude_model("opus"));
        assert!(!is_native_claude_model("gpt-5.3-codex"));
    }

    #[test]
    fn analyzes_initiator_and_vision() {
        let body = serde_json::json!({
            "model": "claude-sonnet-4.6",
            "messages": [
                {"role": "assistant", "content": "hello"},
                {"role": "user", "content": [
                    {"type": "image", "source": {"type": "base64", "data": "AA=="}}
                ]}
            ]
        });
        let metadata = analyze_claude_request(body.to_string().as_bytes(), None).unwrap();
        assert_eq!(metadata.model, "claude-sonnet-4.6");
        assert_eq!(metadata.initiator, "agent");
        assert!(metadata.is_vision);
    }
}
