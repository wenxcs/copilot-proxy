//! Observe cumulative upstream usage without rewriting response bytes.

use serde_json::Value;

const MAX_SSE_EVENT_BYTES: usize = 1024 * 1024;

#[derive(Default)]
pub(crate) struct UsageMeter {
    input: Option<u64>,
    cache_creation_input: Option<u64>,
    cache_read_input: Option<u64>,
    output: Option<u64>,
    reported_total: Option<u64>,
    line: Vec<u8>,
    event_data: Vec<u8>,
    event_bytes: usize,
    line_has_bytes: bool,
    skip_event: bool,
    previous_cr: bool,
}

impl UsageMeter {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn observe_json(&mut self, bytes: &[u8]) {
        let Ok(value) = serde_json::from_slice::<Value>(bytes) else {
            return;
        };
        self.observe_value(&value);
    }

    fn observe_value(&mut self, value: &Value) {
        if let Some(usage) = value.get("usage") {
            self.merge_usage(usage);
        }
        match value.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                if let Some(usage) = value
                    .get("message")
                    .and_then(|message| message.get("usage"))
                {
                    self.merge_usage(usage);
                }
            }
            Some(event) if event.starts_with("response.") => {
                if let Some(usage) = value
                    .get("response")
                    .and_then(|response| response.get("usage"))
                {
                    self.merge_usage(usage);
                }
            }
            _ => {}
        }
    }

    /// Accept arbitrary transport chunks, including splits inside CRLF and JSON.
    pub(crate) fn observe_sse(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if self.previous_cr {
                self.previous_cr = false;
                if byte == b'\n' {
                    continue;
                }
            }
            if matches!(byte, b'\r' | b'\n') {
                self.finish_line();
                self.previous_cr = byte == b'\r';
                continue;
            }
            self.line_has_bytes = true;
            self.event_bytes = self.event_bytes.saturating_add(1);
            if self.event_bytes > MAX_SSE_EVENT_BYTES {
                self.skip_event = true;
                self.line.clear();
                self.event_data.clear();
            } else if !self.skip_event {
                self.line.push(byte);
            }
        }
    }

    /// Process a final event even when the upstream omitted its blank delimiter.
    pub(crate) fn finish(&mut self) {
        if self.line_has_bytes {
            self.finish_line();
        }
        self.finish_event();
        self.previous_cr = false;
    }

    pub(crate) fn input_tokens(&self) -> Option<u64> {
        sum_known([self.input, self.cache_creation_input, self.cache_read_input])
    }

    pub(crate) fn output_tokens(&self) -> Option<u64> {
        self.output
    }

    pub(crate) fn total_tokens(&self) -> Option<u64> {
        let components = sum_known([self.input_tokens(), self.output_tokens()]);
        match (self.reported_total, components) {
            (Some(total), Some(parts)) => Some(total.max(parts)),
            (total, parts) => total.or(parts),
        }
    }

    fn merge_usage(&mut self, usage: &Value) {
        merge_max(
            &mut self.input,
            usage
                .get("prompt_tokens")
                .and_then(Value::as_u64)
                .or_else(|| usage.get("input_tokens").and_then(Value::as_u64)),
        );
        merge_max(
            &mut self.cache_creation_input,
            usage
                .get("cache_creation_input_tokens")
                .and_then(Value::as_u64),
        );
        merge_max(
            &mut self.cache_read_input,
            usage.get("cache_read_input_tokens").and_then(Value::as_u64),
        );
        merge_max(
            &mut self.output,
            usage
                .get("completion_tokens")
                .and_then(Value::as_u64)
                .or_else(|| usage.get("output_tokens").and_then(Value::as_u64)),
        );
        merge_max(
            &mut self.reported_total,
            usage.get("total_tokens").and_then(Value::as_u64),
        );
    }

    fn finish_line(&mut self) {
        if !self.line_has_bytes {
            self.finish_event();
        } else if !self.skip_event
            && let Some(data) = self.line.strip_prefix(b"data:")
        {
            let data = data.strip_prefix(b" ").unwrap_or(data);
            if !self.event_data.is_empty() {
                self.event_data.push(b'\n');
            }
            self.event_data.extend_from_slice(data);
        }
        self.line.clear();
        self.line_has_bytes = false;
    }

    fn finish_event(&mut self) {
        let data = std::mem::take(&mut self.event_data);
        if !self.skip_event && !data.is_empty() && data != b"[DONE]" {
            self.observe_json(&data);
        }
        self.skip_event = false;
        self.event_bytes = 0;
    }
}

fn merge_max(current: &mut Option<u64>, incoming: Option<u64>) {
    if let Some(value) = incoming {
        *current = Some(current.unwrap_or_default().max(value));
    }
}

fn sum_known<const N: usize>(values: [Option<u64>; N]) -> Option<u64> {
    values.into_iter().flatten().reduce(u64::saturating_add)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_openai_json_without_double_counting_nested_details() {
        let mut meter = UsageMeter::new();
        meter.observe_json(br#"{"usage":{"prompt_tokens":12,"completion_tokens":8,"total_tokens":20,"prompt_tokens_details":{"cached_tokens":4},"completion_tokens_details":{"reasoning_tokens":3}}}"#);
        assert_eq!(meter.input_tokens(), Some(12));
        assert_eq!(meter.output_tokens(), Some(8));
        assert_eq!(meter.total_tokens(), Some(20));

        meter.observe_json(br#"{"usage":{"input_tokens":20,"output_tokens":5,"input_tokens_details":{"cached_tokens":10}}}"#);
        assert_eq!(meter.input_tokens(), Some(20));
        assert_eq!(meter.total_tokens(), Some(28));
    }

    #[test]
    fn reads_total_only_or_zero_usage_but_keeps_missing_usage_unknown() {
        let mut meter = UsageMeter::new();
        assert_eq!(meter.total_tokens(), None);
        for bytes in [
            &b"not-json"[..],
            br#"{"usage":null}"#,
            br#"{"error":{"message":"upstream error"}}"#,
            br#"{"content":[{"usage":{"total_tokens":999}}]}"#,
            br#"{"response":{"usage":{"total_tokens":999}}}"#,
            br#"{"type":"content_block_delta","delta":{"text":"usage","usage":{"total_tokens":999}}}"#,
        ] {
            meter.observe_json(bytes);
            assert_eq!(meter.total_tokens(), None);
        }
        meter.observe_json(br#"{"usage":{"total_tokens":0}}"#);
        assert_eq!(meter.total_tokens(), Some(0));
        meter.observe_json(br#"{"usage":{"total_tokens":15}}"#);
        assert_eq!(meter.total_tokens(), Some(15));
        assert_eq!(meter.input_tokens(), None);
        assert_eq!(meter.output_tokens(), None);
    }

    #[test]
    fn merges_native_claude_start_cache_and_cumulative_output() {
        let mut meter = UsageMeter::new();
        meter.observe_json(br#"{"type":"message_start","message":{"usage":{"input_tokens":10,"cache_creation_input_tokens":20,"cache_read_input_tokens":30,"output_tokens":1}}}"#);
        for output in [2, 8, 8, 5] {
            meter.observe_json(
                format!(r#"{{"type":"message_delta","usage":{{"output_tokens":{output}}}}}"#)
                    .as_bytes(),
            );
        }
        assert_eq!(meter.input_tokens(), Some(60));
        assert_eq!(meter.output_tokens(), Some(8));
        assert_eq!(meter.total_tokens(), Some(68));
    }

    #[test]
    fn accepts_fragmented_sse_crlf_multiline_and_repeated_responses_events() {
        let stream = concat!(
            ": heartbeat\r\n\r\n",
            "event: response.completed\r\n",
            "data: {\"type\":\"response.completed\",\r\n",
            "data: \"response\":{\"usage\":{\"input_tokens\":12,\"output_tokens\":8,\"total_tokens\":20}}}\r\n\r\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":12,\"output_tokens\":8,\"total_tokens\":20}}}\n\n",
            "data: [DONE]\n\n",
        );
        for size in [1, 2, 3, 7, 37, stream.len()] {
            let mut meter = UsageMeter::new();
            for chunk in stream.as_bytes().chunks(size) {
                meter.observe_sse(chunk);
            }
            meter.finish();
            meter.finish();
            assert_eq!(meter.total_tokens(), Some(20), "chunk size {size}");
            assert_eq!(meter.input_tokens(), Some(12));
            assert_eq!(meter.output_tokens(), Some(8));
        }
    }

    #[test]
    fn streams_claude_cache_usage_and_handles_unterminated_final_event() {
        let stream = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10,\"cache_creation_input_tokens\":20,\"cache_read_input_tokens\":30,\"output_tokens\":1}}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":9}}",
        );
        let mut meter = UsageMeter::new();
        for chunk in stream.as_bytes().chunks(11) {
            meter.observe_sse(chunk);
        }
        assert_eq!(meter.total_tokens(), Some(61));
        meter.finish();
        assert_eq!(meter.total_tokens(), Some(69));
    }

    #[test]
    fn skips_invalid_and_oversized_sse_frames_then_recovers() {
        let mut meter = UsageMeter::new();
        meter.observe_sse(b"data: invalid json\n\n");
        meter.observe_sse(b"data: ");
        for _ in 0..MAX_SSE_EVENT_BYTES / 1024 + 1 {
            meter.observe_sse(&[b'x'; 1024]);
        }
        // A usage line in the same oversized event must remain discarded.
        meter.observe_sse(b"\ndata: {\"usage\":{\"total_tokens\":999}}\n");
        assert_eq!(meter.total_tokens(), None);
        assert!(meter.line.len() <= MAX_SSE_EVENT_BYTES);
        assert!(meter.event_data.is_empty());
        meter.observe_sse(b"\ndata:{\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":3}}\n\n");
        assert_eq!(meter.total_tokens(), Some(5));
    }

    #[test]
    fn handles_large_unsigned_values_without_overflow_and_ignores_invalid_counts() {
        let mut meter = UsageMeter::new();
        meter.observe_json(
            br#"{"usage":{"input_tokens":-1,"output_tokens":1.5,"total_tokens":"30"}}"#,
        );
        assert_eq!(meter.total_tokens(), None);
        meter.observe_json(
            format!(r#"{{"usage":{{"input_tokens":{},"cache_read_input_tokens":8,"output_tokens":10}}}}"#, u64::MAX).as_bytes(),
        );
        assert_eq!(meter.input_tokens(), Some(u64::MAX));
        assert_eq!(meter.total_tokens(), Some(u64::MAX));
    }
}
