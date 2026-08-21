//! Initiator inference for sticky inference cost savings.
//!
//! This module implements "sticky inference": once a conversation has assistant/tool
//! messages, all subsequent requests are marked as agent-initiated and do not
//! consume Copilot premium requests.
//!
//! Additionally, requests from automated agent systems (e.g. Factory/Droid task
//! workers, Amp subagents) are detected via message content patterns and marked
//! as agent-initiated even on their first turn (which lacks prior assistant
//! messages).

use axum::http::HeaderMap;
use serde_json::Value;

/// Result of analyzing a request body for initiator and vision detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestAnalysis {
    pub initiator: &'static str,
    pub is_vision: bool,
}

/// Infer initiator from native Anthropic message history.
pub fn infer_initiator_claude(messages: &[Value], headers: Option<&HeaderMap>) -> &'static str {
    let initiator = infer_initiator_from_messages(messages, &["assistant", "tool"]);
    if initiator == "user"
        && let Some(headers) = headers
        && ((is_factory_client(headers) && messages_contain_task_marker(messages))
            || (is_amp_client(headers) && messages_contain_amp_subagent_marker(messages)))
    {
        return "agent";
    }
    initiator
}

/// Analyze OpenAI chat completions request for initiator and vision.
pub fn analyze_openai_chat_completions(
    body: &[u8],
    headers: Option<&HeaderMap>,
) -> RequestAnalysis {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return RequestAnalysis {
            initiator: "user",
            is_vision: false,
        };
    };
    let Some(messages) = value.get("messages").and_then(|v| v.as_array()) else {
        return RequestAnalysis {
            initiator: "user",
            is_vision: false,
        };
    };
    let mut initiator = infer_initiator_from_messages(messages, &["assistant", "tool"]);
    if initiator == "user" {
        if let Some(hdrs) = headers {
            if is_factory_client(hdrs) && messages_contain_task_marker(messages) {
                initiator = "agent";
            } else if is_amp_client(hdrs) && messages_contain_amp_subagent_marker(messages) {
                initiator = "agent";
            }
        }
    }
    let is_vision = messages.iter().any(|msg| {
        msg.get("content")
            .and_then(|c| c.as_array())
            .map(|parts| {
                parts
                    .iter()
                    .any(|p| p.get("type") == Some(&Value::String("image_url".to_string())))
            })
            .unwrap_or(false)
    });
    RequestAnalysis {
        initiator,
        is_vision,
    }
}

/// Analyze OpenAI responses API request for initiator and vision.
pub fn analyze_openai_responses(body: &[u8], headers: Option<&HeaderMap>) -> RequestAnalysis {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return RequestAnalysis {
            initiator: "user",
            is_vision: false,
        };
    };
    let Some(input) = value.get("input") else {
        return RequestAnalysis {
            initiator: "user",
            is_vision: false,
        };
    };
    if input.is_string() {
        return RequestAnalysis {
            initiator: "user",
            is_vision: false,
        };
    }
    let Some(items) = input.as_array() else {
        return RequestAnalysis {
            initiator: "user",
            is_vision: false,
        };
    };
    let mut initiator = infer_initiator_from_messages(items, &["assistant", "tool"]);
    if initiator == "user" {
        if let Some(hdrs) = headers {
            if is_factory_client(hdrs) && messages_contain_task_marker(items) {
                initiator = "agent";
            } else if is_amp_client(hdrs) && messages_contain_amp_subagent_marker(items) {
                initiator = "agent";
            }
        }
    }
    let is_vision = items.iter().any(|item| {
        item.get("content")
            .and_then(|c| c.as_array())
            .map(|parts| {
                parts
                    .iter()
                    .any(|p| p.get("type") == Some(&Value::String("input_image".to_string())))
            })
            .unwrap_or(false)
    });
    RequestAnalysis {
        initiator,
        is_vision,
    }
}

fn infer_initiator_from_messages(messages: &[Value], agent_roles: &[&str]) -> &'static str {
    if messages.iter().any(|msg| {
        msg.get("role")
            .and_then(|v| v.as_str())
            .map(|r| agent_roles.contains(&r))
            .unwrap_or(false)
    }) {
        "agent"
    } else {
        "user"
    }
}

/// Patterns in user/system message content that indicate the request originates
/// from an automated task worker or subagent (e.g. Factory mission workers,
/// Task Tool invocations).
const AGENT_TASK_PATTERNS: &[&str] = &[
    "You are a worker assigned to implement feature",
    "## Worker Session",
    "# Task Tool Invocation",
];

/// System prompt patterns that identify Amp subagent sessions (Oracle, Task,
/// code search, diff explainer, Librarian, Walkthrough Planner, REPL, review,
/// etc.).  These are matched against system/developer/user messages.
const AMP_SUBAGENT_PATTERNS: &[&str] = &[
    "You are the Oracle",
    "You are a fast, parallel code search agent",
    "You are a specialized subagent",
    "You are the Librarian",
    "You are the Walkthrough Planner",
    "You are a REPL operator",
];

/// Check whether any message in the request carries content that identifies it
/// as originating from an automated task/worker agent.  This covers both OpenAI
/// (system/user/developer role strings or content arrays) message shapes.
fn messages_contain_task_marker(messages: &[Value]) -> bool {
    for msg in messages {
        // Check both string and array content shapes.
        if let Some(text) = msg.get("content").and_then(|c| c.as_str()) {
            if AGENT_TASK_PATTERNS.iter().any(|p| text.contains(p)) {
                return true;
            }
        }
        if let Some(parts) = msg.get("content").and_then(|c| c.as_array()) {
            for part in parts {
                if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                    if AGENT_TASK_PATTERNS.iter().any(|p| text.contains(p)) {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// Returns `true` when the incoming request headers identify the caller as a
/// Factory / Droid CLI client (via the `x-factory-client` header).
pub fn is_factory_client(headers: &HeaderMap) -> bool {
    headers.contains_key("x-factory-client")
}

/// Returns `true` when the incoming request headers identify the caller as an
/// Amp CLI/IDE client (via the `x-amp-thread-id` header).
pub fn is_amp_client(headers: &HeaderMap) -> bool {
    headers.contains_key("x-amp-thread-id")
}

/// Check whether any message in the request carries system prompt content that
/// identifies it as originating from an Amp subagent (Oracle, Task, code
/// search, etc.). Checks OpenAI string and content-array message shapes.
fn messages_contain_amp_subagent_marker(messages: &[Value]) -> bool {
    for msg in messages {
        // Check both string and array content shapes.
        if let Some(text) = msg.get("content").and_then(|c| c.as_str()) {
            if AMP_SUBAGENT_PATTERNS.iter().any(|p| text.contains(p)) {
                return true;
            }
        }
        if let Some(parts) = msg.get("content").and_then(|c| c.as_array()) {
            for part in parts {
                if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                    if AMP_SUBAGENT_PATTERNS.iter().any(|p| text.contains(p)) {
                        return true;
                    }
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_claude_initiator() {
        let messages = vec![
            json!({"role": "user", "content": "Hello"}),
            json!({"role": "assistant", "content": "Hi"}),
        ];
        assert_eq!(infer_initiator_claude(&messages, None), "agent");
    }

    #[test]
    fn test_openai_user_only() {
        let body = json!({"messages": [{"role": "user", "content": "Hello"}]});
        let result = analyze_openai_chat_completions(body.to_string().as_bytes(), None);
        assert_eq!(result.initiator, "user");
        assert!(!result.is_vision);
    }

    #[test]
    fn test_openai_with_assistant() {
        let body = json!({
            "messages": [
                {"role": "user", "content": "Hello"},
                {"role": "assistant", "content": "Hi"}
            ]
        });
        let result = analyze_openai_chat_completions(body.to_string().as_bytes(), None);
        assert_eq!(result.initiator, "agent");
        assert!(!result.is_vision);
    }

    #[test]
    fn test_openai_with_tool() {
        let body = json!({
            "messages": [
                {"role": "user", "content": "Hello"},
                {"role": "tool", "tool_call_id": "123", "content": "result"}
            ]
        });
        let result = analyze_openai_chat_completions(body.to_string().as_bytes(), None);
        assert_eq!(result.initiator, "agent");
        assert!(!result.is_vision);
    }

    #[test]
    fn test_openai_invalid_json() {
        let result = analyze_openai_chat_completions(b"not json", None);
        assert_eq!(result.initiator, "user");
        assert!(!result.is_vision);
    }

    #[test]
    fn test_openai_missing_messages() {
        let body = json!({"model": "gpt-4"});
        let result = analyze_openai_chat_completions(body.to_string().as_bytes(), None);
        assert_eq!(result.initiator, "user");
        assert!(!result.is_vision);
    }

    #[test]
    fn test_openai_with_vision() {
        let body = json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "What's in this image?"},
                    {"type": "image_url", "image_url": {"url": "https://example.com/image.png"}}
                ]
            }]
        });
        let result = analyze_openai_chat_completions(body.to_string().as_bytes(), None);
        assert_eq!(result.initiator, "user");
        assert!(result.is_vision);
    }

    #[test]
    fn test_responses_string_input() {
        let body = json!({"input": "Hello, world!"});
        let result = analyze_openai_responses(body.to_string().as_bytes(), None);
        assert_eq!(result.initiator, "user");
        assert!(!result.is_vision);
    }

    #[test]
    fn test_responses_user_only() {
        let body = json!({
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "Hello"}]}]
        });
        let result = analyze_openai_responses(body.to_string().as_bytes(), None);
        assert_eq!(result.initiator, "user");
        assert!(!result.is_vision);
    }

    #[test]
    fn test_responses_with_assistant() {
        let body = json!({
            "input": [
                {"role": "user", "content": [{"type": "input_text", "text": "Hello"}]},
                {"role": "assistant", "content": [{"type": "output_text", "text": "Hi"}]}
            ]
        });
        let result = analyze_openai_responses(body.to_string().as_bytes(), None);
        assert_eq!(result.initiator, "agent");
        assert!(!result.is_vision);
    }

    #[test]
    fn test_responses_with_tool() {
        let body = json!({
            "input": [
                {"role": "user", "content": "Hello"},
                {"role": "tool", "content": "result"}
            ]
        });
        let result = analyze_openai_responses(body.to_string().as_bytes(), None);
        assert_eq!(result.initiator, "agent");
        assert!(!result.is_vision);
    }

    #[test]
    fn test_responses_invalid_json() {
        let result = analyze_openai_responses(b"not json", None);
        assert_eq!(result.initiator, "user");
        assert!(!result.is_vision);
    }

    #[test]
    fn test_responses_missing_input() {
        let body = json!({"model": "gpt-4"});
        let result = analyze_openai_responses(body.to_string().as_bytes(), None);
        assert_eq!(result.initiator, "user");
        assert!(!result.is_vision);
    }

    #[test]
    fn test_responses_with_vision() {
        let body = json!({
            "input": [{
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "What's in this image?"},
                    {"type": "input_image", "image_url": "https://example.com/image.png"}
                ]
            }]
        });
        let result = analyze_openai_responses(body.to_string().as_bytes(), None);
        assert_eq!(result.initiator, "user");
        assert!(result.is_vision);
    }

    fn factory_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("x-factory-client", "cli".parse().unwrap());
        headers
    }

    // --- Factory task worker detection tests ---

    #[test]
    fn test_factory_task_worker_openai_chat() {
        // Worker marker in an OpenAI chat completions request
        let body = json!({
            "messages": [{
                "role": "user",
                "content": "You are a worker assigned to implement feature \"setup\".\n## Worker Session"
            }]
        });
        let headers = factory_headers();
        let result = analyze_openai_chat_completions(body.to_string().as_bytes(), Some(&headers));
        assert_eq!(result.initiator, "agent");
    }

    #[test]
    fn test_factory_task_worker_openai_responses() {
        // Worker marker in an OpenAI responses API request
        let body = json!({
            "input": [{
                "role": "user",
                "content": "You are a worker assigned to implement feature \"setup\".\n## Worker Session"
            }]
        });
        let headers = factory_headers();
        let result = analyze_openai_responses(body.to_string().as_bytes(), Some(&headers));
        assert_eq!(result.initiator, "agent");
    }

    #[test]
    fn test_task_marker_string_content() {
        let messages = vec![json!({
            "role": "user",
            "content": "You are a worker assigned to implement feature \"x\"."
        })];
        assert!(messages_contain_task_marker(&messages));
    }

    #[test]
    fn test_task_marker_array_content() {
        let messages = vec![json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "Hello"},
                {"type": "text", "text": "## Worker Session\nYour id is abc"}
            ]
        })];
        assert!(messages_contain_task_marker(&messages));
    }

    #[test]
    fn test_no_task_marker() {
        let messages = vec![json!({"role": "user", "content": "Hello world"})];
        assert!(!messages_contain_task_marker(&messages));
    }

    #[test]
    fn test_factory_subagent_openai_responses() {
        let body = json!({
            "input": [{
                "role": "user",
                "content": "# Task Tool Invocation\n\nSubagent type: scrutiny-feature-reviewer"
            }]
        });
        let headers = factory_headers();
        let result = analyze_openai_responses(body.to_string().as_bytes(), Some(&headers));
        assert_eq!(result.initiator, "agent");
    }

    #[test]
    fn test_task_marker_subagent() {
        let messages = vec![json!({
            "role": "user",
            "content": "# Task Tool Invocation\n\nSubagent type: worker"
        })];
        assert!(messages_contain_task_marker(&messages));
    }

    // --- Amp subagent detection tests ---

    fn amp_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("x-amp-thread-id", "T-019d90e9-test".parse().unwrap());
        headers.insert("x-amp-feature", "amp.chat".parse().unwrap());
        headers
    }

    #[test]
    fn test_amp_oracle_openai_chat() {
        let body = json!({
            "messages": [{
                "role": "system",
                "content": "You are the Oracle - an expert AI advisor with advanced reasoning capabilities."
            }, {
                "role": "user",
                "content": "Check this code for correctness"
            }]
        });
        let headers = amp_headers();
        let result = analyze_openai_chat_completions(body.to_string().as_bytes(), Some(&headers));
        assert_eq!(result.initiator, "agent");
    }

    #[test]
    fn test_amp_code_search_openai_responses() {
        let body = json!({
            "input": [{
                "role": "system",
                "content": "You are a fast, parallel code search agent."
            }, {
                "role": "user",
                "content": "Find all usages of struct Foo"
            }]
        });
        let headers = amp_headers();
        let result = analyze_openai_responses(body.to_string().as_bytes(), Some(&headers));
        assert_eq!(result.initiator, "agent");
    }

    #[test]
    fn test_amp_subagent_marker_detection() {
        let messages = vec![json!({
            "role": "user",
            "content": "You are the Oracle - an expert AI advisor."
        })];
        assert!(messages_contain_amp_subagent_marker(&messages));
    }

    #[test]
    fn test_amp_subagent_marker_array_content() {
        let messages = vec![json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "You are a specialized subagent that explains diffs."}
            ]
        })];
        assert!(messages_contain_amp_subagent_marker(&messages));
    }

    #[test]
    fn test_no_amp_subagent_marker() {
        let messages = vec![json!({"role": "user", "content": "Hello world"})];
        assert!(!messages_contain_amp_subagent_marker(&messages));
    }

    #[test]
    fn test_is_amp_client() {
        let headers = amp_headers();
        assert!(is_amp_client(&headers));
    }

    #[test]
    fn test_is_not_amp_client() {
        let headers = HeaderMap::new();
        assert!(!is_amp_client(&headers));
    }
}
