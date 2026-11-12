//! Forward responses while settling token budgets before terminal stream events.

use crate::error::Error;
use crate::model_policy::UsagePermit;
use crate::usage::UsageMeter;
use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use futures::{Stream, StreamExt};
use serde_json::Value;
use std::pin::Pin;

const MAX_SSE_FRAME_BYTES: usize = 1024 * 1024;
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

pub(crate) async fn forward_response(
    response: reqwest::Response,
    permit: UsagePermit,
    native: bool,
) -> Result<Response, Error> {
    if !response.status().is_success() {
        permit.finish(Some(0))?;
        return crate::proxy::forward_response(response).await;
    }

    let status = response.status();
    let headers = response.headers().clone();
    let streaming = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream"));
    let builder = response_builder(status, &headers, streaming);
    let body = if streaming {
        let state = MeteredSse::new(Box::pin(response.bytes_stream()), permit, native);
        let stream = futures::stream::unfold(state, |mut state| async move {
            state
                .next_frame()
                .await
                .map(|bytes| (Ok::<Bytes, std::io::Error>(bytes), state))
        });
        Body::from_stream(stream)
    } else {
        let bytes = response.bytes().await?;
        let mut meter = UsageMeter::new();
        meter.observe_json(&bytes);
        let complete = serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|value| value.get("usage").map(complete_usage))
            .unwrap_or(false);
        let Some(total) = meter.total_tokens().filter(|_| complete) else {
            permit.finish(None)?;
            return Err(Error::UsageUnavailable);
        };
        permit.finish(Some(total))?;
        Body::from(bytes)
    };
    builder
        .body(body)
        .map_err(|error| Error::InvalidRequest(error.to_string()))
}

fn response_builder(
    status: StatusCode,
    headers: &HeaderMap,
    streaming: bool,
) -> axum::http::response::Builder {
    let mut builder = Response::builder().status(status);
    for (name, value) in headers {
        if !HOP_BY_HOP.contains(&name.as_str()) && !(streaming && name == header::CONTENT_LENGTH) {
            builder = builder.header(name, value);
        }
    }
    if streaming && !headers.contains_key(header::CACHE_CONTROL) {
        builder = builder.header(header::CACHE_CONTROL, "no-cache");
    }
    builder
}

type Upstream = Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

struct MeteredSse {
    upstream: Upstream,
    permit: Option<UsagePermit>,
    native: bool,
    meter: UsageMeter,
    scanner: FrameScanner,
    pending: Bytes,
    offset: usize,
    last_recorded: Option<u64>,
    final_usage_seen: bool,
    eof: bool,
    done: bool,
}

impl MeteredSse {
    fn new(upstream: Upstream, permit: UsagePermit, native: bool) -> Self {
        Self {
            upstream,
            permit: Some(permit),
            native,
            meter: UsageMeter::new(),
            scanner: FrameScanner::default(),
            pending: Bytes::new(),
            offset: 0,
            last_recorded: None,
            final_usage_seen: false,
            eof: false,
            done: false,
        }
    }

    async fn next_frame(&mut self) -> Option<Bytes> {
        if self.done {
            return None;
        }
        loop {
            if self.offset < self.pending.len() {
                match self.scanner.read(&self.pending[self.offset..]) {
                    Ok((consumed, frame)) => {
                        self.offset += consumed;
                        if let Some(frame) = frame {
                            return Some(self.process_frame(frame));
                        }
                    }
                    Err(()) => return Some(self.abort(Error::UsageUnavailable)),
                }
                continue;
            }
            if self.eof {
                if let Some(frame) = self.scanner.finish() {
                    return Some(self.process_frame(frame));
                }
                // An EOF without the protocol's terminal event cannot confirm final usage.
                return Some(self.abort(Error::UsageUnavailable));
            }
            match self.upstream.next().await {
                Some(Ok(chunk)) => {
                    self.pending = chunk;
                    self.offset = 0;
                }
                Some(Err(error)) => return Some(self.abort(Error::Upstream(error))),
                None => self.eof = true,
            }
        }
    }

    fn process_frame(&mut self, frame: Bytes) -> Bytes {
        self.meter.observe_sse(&frame);
        self.meter.finish();
        let (event, data) = event_data(&frame);
        let value = serde_json::from_slice::<Value>(&data).ok();
        let kind = value
            .as_ref()
            .and_then(|value| value.get("type"))
            .and_then(Value::as_str)
            .or(event.as_deref());
        if let Some(value) = &value {
            if self.native {
                if kind == Some("message_delta")
                    && value
                        .get("usage")
                        .and_then(|usage| usage.get("output_tokens"))
                        .and_then(Value::as_u64)
                        .is_some()
                    && self.meter.input_tokens().is_some()
                {
                    self.final_usage_seen = true;
                }
            } else if value.get("usage").is_some_and(complete_usage)
                || (is_response_terminal(kind)
                    && value
                        .get("response")
                        .and_then(|response| response.get("usage"))
                        .is_some_and(complete_usage))
            {
                self.final_usage_seen = true;
            }
        }
        if let Some(total) = self.meter.total_tokens()
            && self.last_recorded != Some(total)
        {
            self.last_recorded = Some(total);
            if let Some(permit) = self.permit.as_mut()
                && let Err(error) = permit.record(total)
                && !matches!(&error, Error::LimitExceeded { .. })
            {
                return self.abort(error);
            }
        }

        let terminal = data.trim_ascii() == b"[DONE]"
            || kind == Some("message_stop")
            || is_response_terminal(kind);
        if terminal {
            let total = self.meter.total_tokens().filter(|_| self.final_usage_seen);
            let Some(total) = total else {
                return self.abort(Error::UsageUnavailable);
            };
            self.done = true;
            if let Some(permit) = self.permit.take()
                && let Err(error) = permit.finish(Some(total))
            {
                return error_event(&error, self.native);
            }
        } else if kind == Some("error")
            || value
                .as_ref()
                .is_some_and(|value| value.get("error").is_some())
        {
            // Preserve a genuine upstream error while treating its usage as unknown.
            self.exhaust_unknown();
            self.done = true;
        }
        frame
    }

    fn abort(&mut self, error: Error) -> Bytes {
        self.exhaust_unknown();
        self.done = true;
        error_event(&error, self.native)
    }

    fn exhaust_unknown(&mut self) {
        if let Some(permit) = self.permit.take()
            && let Err(error) = permit.finish(None)
        {
            tracing::error!(%error, "Failed to persist unknown streamed usage");
        }
    }
}

fn complete_usage(usage: &Value) -> bool {
    usage.get("total_tokens").and_then(Value::as_u64).is_some()
        || (usage
            .get("input_tokens")
            .or_else(|| usage.get("prompt_tokens"))
            .and_then(Value::as_u64)
            .is_some()
            && usage
                .get("output_tokens")
                .or_else(|| usage.get("completion_tokens"))
                .and_then(Value::as_u64)
                .is_some())
}

fn is_response_terminal(kind: Option<&str>) -> bool {
    matches!(
        kind,
        Some("response.completed" | "response.failed" | "response.incomplete")
    )
}

fn error_event(error: &Error, native: bool) -> Bytes {
    let body = if native {
        serde_json::json!({
            "type": "error",
            "error": {
                "type": if error.status_and_type().0 == StatusCode::TOO_MANY_REQUESTS {
                    "rate_limit_error"
                } else {
                    "api_error"
                },
                "message": error.to_string(),
            }
        })
    } else {
        error.body()
    };
    Bytes::from(format!("event: error\ndata: {body}\n\n"))
}

fn event_data(frame: &[u8]) -> (Option<String>, Vec<u8>) {
    let mut event = None;
    let mut data = Vec::new();
    for line in frame.split(|&byte| matches!(byte, b'\r' | b'\n')) {
        if let Some(value) = line.strip_prefix(b"data:") {
            if !data.is_empty() {
                data.push(b'\n');
            }
            data.extend_from_slice(value.strip_prefix(b" ").unwrap_or(value));
        } else if let Some(value) = line.strip_prefix(b"event:") {
            event = std::str::from_utf8(value.strip_prefix(b" ").unwrap_or(value))
                .ok()
                .map(str::to_string);
        }
    }
    (event, data)
}

/// Keep one raw frame and scan every transport byte once. CRLF may span chunks.
#[derive(Default)]
struct FrameScanner {
    raw: Vec<u8>,
    line_has_bytes: bool,
    pending_cr: bool,
}

impl FrameScanner {
    fn read(&mut self, chunk: &[u8]) -> Result<(usize, Option<Bytes>), ()> {
        let mut consumed = 0;
        while let Some(&byte) = chunk.get(consumed) {
            if self.pending_cr {
                self.pending_cr = false;
                let boundary = self.line_end();
                if byte == b'\n' {
                    self.push(byte)?;
                    consumed += 1;
                    if boundary {
                        return Ok((consumed, Some(self.take())));
                    }
                    continue;
                }
                if boundary {
                    return Ok((consumed, Some(self.take())));
                }
            }
            self.push(byte)?;
            consumed += 1;
            match byte {
                b'\r' => self.pending_cr = true,
                b'\n' if self.line_end() => return Ok((consumed, Some(self.take()))),
                b'\n' => {}
                _ => self.line_has_bytes = true,
            }
        }
        Ok((consumed, None))
    }

    fn finish(&mut self) -> Option<Bytes> {
        if self.pending_cr {
            self.pending_cr = false;
            self.line_end();
        }
        (!self.raw.is_empty()).then(|| self.take())
    }

    fn push(&mut self, byte: u8) -> Result<(), ()> {
        if self.raw.len() >= MAX_SSE_FRAME_BYTES {
            return Err(());
        }
        self.raw.push(byte);
        Ok(())
    }

    fn line_end(&mut self) -> bool {
        let boundary = !self.line_has_bytes;
        self.line_has_bytes = false;
        boundary
    }

    fn take(&mut self) -> Bytes {
        Bytes::from(std::mem::take(&mut self.raw))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_policy::ModelPolicy;
    use axum::body::to_bytes;
    use std::sync::Arc;

    fn budget(limit: u64) -> (Arc<ModelPolicy>, UsagePermit) {
        let policy = ModelPolicy::new(&format!("5h:test-model:{limit}"), None).unwrap();
        let permit = policy.begin("test-model", true).unwrap().unwrap();
        (policy, permit)
    }

    fn upstream(bytes: &[u8], streaming: bool, chunk_size: usize) -> reqwest::Response {
        let chunks: Vec<_> = bytes
            .chunks(chunk_size)
            .map(|chunk| Ok::<_, std::io::Error>(Bytes::copy_from_slice(chunk)))
            .collect();
        reqwest::Response::from(
            axum::http::Response::builder()
                .header(
                    header::CONTENT_TYPE,
                    if streaming {
                        "Text/Event-Stream; charset=utf-8"
                    } else {
                        "application/json"
                    },
                )
                .header(header::CONTENT_LENGTH, bytes.len())
                .header(header::CONNECTION, "keep-alive")
                .header("x-upstream-id", "example")
                .body(reqwest::Body::wrap_stream(futures::stream::iter(chunks)))
                .unwrap(),
        )
    }

    fn assert_exhausted(policy: &Arc<ModelPolicy>) {
        assert!(matches!(
            policy.begin("test-model", true),
            Err(Error::LimitExceeded { .. })
        ));
    }

    #[tokio::test]
    async fn forwards_json_bytes_and_end_to_end_headers_then_accounts_usage() {
        let (policy, permit) = budget(20);
        let json = br#"{ "usage": {"input_tokens":3,"output_tokens":6},"output":[] }"#;
        let response = forward_response(upstream(json, false, 3), permit, false)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-upstream-id"], "example");
        assert_eq!(
            response.headers()[header::CONTENT_LENGTH],
            json.len().to_string()
        );
        assert!(!response.headers().contains_key(header::CONNECTION));
        assert_eq!(
            to_bytes(response.into_body(), 4096).await.unwrap().as_ref(),
            json
        );
        policy
            .begin("test-model", true)
            .unwrap()
            .unwrap()
            .finish(Some(0))
            .unwrap();
    }

    #[tokio::test]
    async fn rejects_over_budget_or_unknown_json_usage() {
        for (json, missing) in [
            (&br#"{"usage":{"total_tokens":11}}"#[..], false),
            (&br#"{"output":[]}"#[..], true),
            (&br#"{"usage":{"input_tokens":4}}"#[..], true),
            (&br#"{"usage":{"output_tokens":4}}"#[..], true),
        ] {
            let (policy, permit) = budget(10);
            let error = forward_response(upstream(json, false, 3), permit, false)
                .await
                .unwrap_err();
            if missing {
                assert!(matches!(error, Error::UsageUnavailable));
            } else {
                assert!(matches!(error, Error::LimitExceeded { used: 11, .. }));
            }
            assert_exhausted(&policy);
        }
    }

    #[tokio::test]
    async fn preserves_upstream_http_errors_without_charging_tokens() {
        let (policy, permit) = budget(10);
        let response = reqwest::Response::from(
            axum::http::Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header("x-upstream-error", "preserved")
                .body(reqwest::Body::from("upstream error body"))
                .unwrap(),
        );
        let response = forward_response(response, permit, false).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(response.headers()["x-upstream-error"], "preserved");
        assert_eq!(
            to_bytes(response.into_body(), 4096).await.unwrap(),
            "upstream error body"
        );
        policy
            .begin("test-model", true)
            .unwrap()
            .unwrap()
            .finish(Some(0))
            .unwrap();
    }

    #[tokio::test]
    async fn preserves_fragmented_stream_frames_exactly_with_complete_usage() {
        let stream = concat!(
            ": comment\r\n\r\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}],\"usage\":null}\r\n\r\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":6,\"total_tokens\":9}}\r\n\r\n",
            "data: [DONE]\r\n\r\n",
        );
        for chunk_size in [1, 2, 7, stream.len()] {
            let (policy, permit) = budget(20);
            let response =
                forward_response(upstream(stream.as_bytes(), true, chunk_size), permit, false)
                    .await
                    .unwrap();
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-cache");
            assert!(!response.headers().contains_key(header::CONTENT_LENGTH));
            assert_eq!(to_bytes(response.into_body(), 4096).await.unwrap(), stream);
            policy
                .begin("test-model", true)
                .unwrap()
                .unwrap()
                .finish(Some(0))
                .unwrap();
        }
    }

    #[tokio::test]
    async fn replaces_chat_terminal_with_limit_error_before_clients_stop_reading() {
        let stream = concat!(
            "data: {\"usage\":{\"total_tokens\":11}}\n\n",
            "data: [DONE]\n\n",
        );
        let (policy, permit) = budget(10);
        let response = forward_response(upstream(stream.as_bytes(), true, 1), permit, false)
            .await
            .unwrap();
        let bytes = to_bytes(response.into_body(), 4096).await.unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(body.contains("event: error\n"));
        assert!(body.contains("model_token_limit_exceeded"));
        assert!(!body.contains("[DONE]"));
        assert_exhausted(&policy);
    }

    #[tokio::test]
    async fn accounts_responses_terminal_usage_before_forwarding_completed() {
        for limit in [10, 20] {
            let stream = concat!(
                "event: response.output_text.delta\n",
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
                "event: response.completed\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":7,\"output_tokens\":8}}}\n\n",
            );
            let (policy, permit) = budget(limit);
            let response = forward_response(upstream(stream.as_bytes(), true, 2), permit, false)
                .await
                .unwrap();
            let bytes = to_bytes(response.into_body(), 4096).await.unwrap();
            if limit == 10 {
                let body = String::from_utf8_lossy(&bytes);
                assert!(body.contains("model_token_limit_exceeded"));
                assert!(!body.contains("response.completed"));
                assert_exhausted(&policy);
            } else {
                assert_eq!(bytes, stream);
            }
        }
    }

    #[tokio::test]
    async fn native_stream_requires_final_output_usage_and_shapes_errors_as_anthropic() {
        let start = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n";
        let delta = "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":4}}\n\n";
        let stop = "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        for (stream, limit, expected) in [
            (format!("{start}{delta}{stop}"), 10, None),
            (format!("{start}{delta}{stop}"), 5, Some("rate_limit_error")),
            (format!("{start}{stop}"), 10, Some("api_error")),
            (format!("{start}{delta}"), 10, Some("api_error")),
            (stop.to_string(), 10, Some("api_error")),
        ] {
            let (policy, permit) = budget(limit);
            let response = forward_response(upstream(stream.as_bytes(), true, 3), permit, true)
                .await
                .unwrap();
            let bytes = to_bytes(response.into_body(), 4096).await.unwrap();
            if let Some(expected) = expected {
                let body = String::from_utf8_lossy(&bytes);
                let (_, error_data) = body.rsplit_once("event: error\ndata: ").unwrap();
                let error: Value = serde_json::from_str(error_data.trim()).unwrap();
                assert_eq!(error["type"], "error");
                assert_eq!(error["error"]["type"], expected);
                assert!(!body.contains("event: message_stop"));
                assert_exhausted(&policy);
            } else {
                assert_eq!(bytes, stream);
            }
        }
    }

    #[tokio::test]
    async fn dropped_or_oversized_streams_cannot_release_unverified_budget() {
        let (policy, permit) = budget(10);
        let response = forward_response(upstream(b"data: pending\n\n", true, 3), permit, false)
            .await
            .unwrap();
        drop(response);
        assert_exhausted(&policy);

        let (policy, permit) = budget(10);
        let bytes = vec![b'x'; MAX_SSE_FRAME_BYTES + 1];
        let response = forward_response(upstream(&bytes, true, 128), permit, false)
            .await
            .unwrap();
        let bytes = to_bytes(response.into_body(), 4096).await.unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("usage_unavailable"));
        assert_exhausted(&policy);
    }

    #[tokio::test]
    async fn stream_transport_errors_are_visible_and_exhaust_unknown_usage() {
        let (policy, permit) = budget(10);
        let stream = futures::stream::iter([
            Ok(Bytes::from_static(b": comment\n\n")),
            Err(std::io::Error::other("simulated upstream disconnect")),
        ]);
        let response = reqwest::Response::from(
            axum::http::Response::builder()
                .header(header::CONTENT_TYPE, "text/event-stream")
                .body(reqwest::Body::wrap_stream(stream))
                .unwrap(),
        );
        let response = forward_response(response, permit, false).await.unwrap();
        let bytes = to_bytes(response.into_body(), 4096).await.unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(body.starts_with(": comment\n\n"));
        assert!(body.contains("event: error"));
        assert!(body.contains("upstream_error"));
        assert_exhausted(&policy);
    }

    #[test]
    fn frame_scanner_preserves_mixed_line_endings_and_chunk_boundaries() {
        let raw = b"data: one\r\n\r\ndata: two\r\rdata: three\n\ndata: final";
        for size in [1, 2, 3, 5, raw.len()] {
            let mut scanner = FrameScanner::default();
            let mut frames = Vec::new();
            for chunk in raw.chunks(size) {
                let mut offset = 0;
                while offset < chunk.len() {
                    let (consumed, frame) = scanner.read(&chunk[offset..]).unwrap();
                    offset += consumed;
                    if let Some(frame) = frame {
                        frames.push(frame);
                    }
                }
            }
            frames.extend(scanner.finish());
            assert_eq!(frames.len(), 4, "chunk size {size}");
            assert_eq!(frames.concat(), raw);
            let (_, data) = event_data(&frames[0]);
            assert_eq!(data, b"one");
        }
    }
}
