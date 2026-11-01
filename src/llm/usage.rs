use serde_json::Value;

use crate::llm::provider::Protocol;

/// 尾包缓冲上限：非流式 JSON 的 usage 在包尾，SSE 的 usage 在最后几个 data 块。
const TAIL_CAP: usize = 64 * 1024;

/// 只保留尾部 64 KiB 的追加缓冲。`push` 超过 cap 时从前截断。
#[derive(Debug)]
pub struct TailBuffer {
    buf: Vec<u8>,
}

impl Default for TailBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl TailBuffer {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn push(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
        if self.buf.len() > TAIL_CAP {
            let drop_n = self.buf.len() - TAIL_CAP;
            self.buf.drain(0..drop_n);
        }
    }

    pub fn tail(&self) -> &[u8] {
        &self.buf
    }
}

/// 从响应尾包提取 token 总数。**best-effort，永不失败**（解析不到返回 0）。
/// `systemone` 为桩：恒返回 0（ADR-L8）。
pub fn extract_tokens(protocol: Protocol, content_type: Option<&str>, tail: &[u8]) -> u64 {
    match protocol {
        Protocol::Systemone => 0,
        protocol => {
            let is_sse = content_type
                .map(|c| c.contains("text/event-stream"))
                .unwrap_or(false);
            if is_sse {
                extract_sse(protocol, tail)
            } else {
                find_usage_object(tail)
                    .map(|v| usage_tokens(&v))
                    .unwrap_or(0)
            }
        }
    }
}

/// 用法对象的 token 总数：openai 取 `total_tokens`，否则 `prompt_tokens +
/// completion_tokens`；anthropic 的 `input_tokens`/`output_tokens` 走同一回退。
fn usage_tokens(obj: &Value) -> u64 {
    let get = |k: &str| obj.get(k).and_then(|v| v.as_u64());
    if let Some(total) = get("total_tokens") {
        return total;
    }
    let input = get("prompt_tokens")
        .or_else(|| get("input_tokens"))
        .unwrap_or(0);
    let output = get("completion_tokens")
        .or_else(|| get("output_tokens"))
        .unwrap_or(0);
    input + output
}

/// SSE：逐行取 `data:` 前缀的 JSON，汇总所有含 `usage` 的块。
/// - openai：每块 usage 计入 total_tokens（或 prompt+completion）。
/// - anthropic：`input_tokens` 取首个出现的值，`output_tokens` 取最后一个
///   （message_delta 的 usage 是累计值）。
fn extract_sse(protocol: Protocol, tail: &[u8]) -> u64 {
    let mut usages: Vec<Value> = Vec::new();
    for line in tail.split(|&b| b == b'\n') {
        let Ok(s) = std::str::from_utf8(line) else {
            continue;
        };
        let Some(s) = s.strip_prefix("data:") else {
            continue;
        };
        let s = s.trim_start_matches(' ').trim();
        if s.is_empty() || s == "[DONE]" {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(s) else {
            continue;
        };
        collect_usages(&v, &mut usages);
    }
    match protocol {
        Protocol::Openai => usages.iter().map(usage_tokens).sum(),
        Protocol::Anthropic => {
            let input = usages
                .iter()
                .find_map(|u| u.get("input_tokens").and_then(|v| v.as_u64()))
                .unwrap_or(0);
            let output = usages
                .iter()
                .rev()
                .find_map(|u| u.get("output_tokens").and_then(|v| v.as_u64()))
                .unwrap_or(0);
            input + output
        }
        Protocol::Systemone => 0,
    }
}

/// 深度收集 JSON 值里的所有 `usage` 对象（openai 的 usage 在 chunk 根上，
/// anthropic 的 message_start 里嵌在 `message` 下）。不再下钻已收集的 `usage`
/// 子树，避免重复计数。
fn collect_usages(v: &Value, out: &mut Vec<Value>) {
    match v {
        Value::Object(map) => {
            for (k, child) in map {
                if k == "usage" {
                    if child.is_object() {
                        out.push(child.clone());
                    }
                } else {
                    collect_usages(child, out);
                }
            }
        }
        Value::Array(arr) => {
            for child in arr {
                collect_usages(child, out);
            }
        }
        _ => {}
    }
}

/// 逐字节查找带引号的 `"usage"` 字面量，其后跳过空白与 `:`，以花括号深度计数
/// （含字符串状态机）截取完整 JSON 对象后反序列化；失败则继续找下一个。
fn find_usage_object(tail: &[u8]) -> Option<Value> {
    let s = std::str::from_utf8(tail).ok()?;
    let key = "\"usage\"";
    let mut from = 0usize;
    while from < s.len() {
        let rel = s[from..].find(key)?;
        let after_key = from + rel + key.len();
        let rest = s[after_key..].trim_start();
        let Some(obj_start) = rest.find('{') else {
            from = after_key;
            continue;
        };
        let body = &rest[obj_start..];
        if let Some(end) = matching_brace(body) {
            if let Ok(v) = serde_json::from_str::<Value>(&body[..=end]) {
                if v.is_object() {
                    return Some(v);
                }
            }
        }
        from = after_key + obj_start + 1;
    }
    None
}

/// 从首个 `{` 起做深度计数，`}` 回到 0 的位置即对象末尾。字符串与转义状态机
/// 防止 `"a}"` 里的括号被误数。
fn matching_brace(s: &str) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape = false;
    for (i, ch) in s.char_indices() {
        if in_string {
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_string = false;
            }
        } else {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                '"' => in_string = true,
                _ => {}
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_buffer_keeps_only_the_tail() {
        let mut buf = TailBuffer::new();
        buf.push(b"head|");
        buf.push(&[b'x'; 100]);
        assert!(buf.tail().len() <= super::TAIL_CAP);
        assert!(buf.tail().ends_with(b"xxx"));
    }

    #[test]
    fn openai_json_usage_from_tail() {
        let body = br#"{"id":"x","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#;
        assert_eq!(
            extract_tokens(Protocol::Openai, Some("application/json"), body),
            15
        );
    }

    #[test]
    fn openai_json_usage_without_total_sums_parts() {
        let body = br#"{"usage":{"prompt_tokens":4,"completion_tokens":6}}"#;
        assert_eq!(
            extract_tokens(Protocol::Openai, Some("application/json"), body),
            10
        );
    }

    #[test]
    fn anthropic_json_usage_uses_input_and_output() {
        let body = br#"{"usage":{"input_tokens":3,"output_tokens":7}}"#;
        assert_eq!(
            extract_tokens(Protocol::Anthropic, Some("application/json"), body),
            10
        );
    }

    #[test]
    fn anthropic_sse_takes_first_input_and_last_output() {
        let body = b"event: message_start\n\
                     data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n\
                     event: message_delta\n\
                     data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":7}}\n\n";
        assert_eq!(
            extract_tokens(Protocol::Anthropic, Some("text/event-stream"), body),
            10
        );
    }

    #[test]
    fn openai_sse_sums_usage_chunks() {
        let body = b"data: {\"choices\":[]}\n\n\
                     data: {\"choices\":[],\"usage\":{\"total_tokens\":15}}\n\n\
                     data: [DONE]\n\n";
        assert_eq!(
            extract_tokens(Protocol::Openai, Some("text/event-stream"), body),
            15
        );
    }

    #[test]
    fn missing_usage_counts_zero() {
        let body = br#"{"ok":true}"#;
        assert_eq!(
            extract_tokens(Protocol::Openai, Some("application/json"), body),
            0
        );
        assert_eq!(extract_tokens(Protocol::Openai, None, b""), 0);
    }

    #[test]
    fn systemone_stub_always_zero() {
        let body = br#"{"usage":{"input_tokens":50,"output_tokens":50}}"#;
        assert_eq!(
            extract_tokens(Protocol::Systemone, Some("application/json"), body),
            0
        );
        let sse = b"data: {\"usage\":{\"total_tokens\":99}}\n\n";
        assert_eq!(
            extract_tokens(Protocol::Systemone, Some("text/event-stream"), sse),
            0
        );
    }

    #[test]
    fn usage_after_truncated_tail_still_found() {
        // 尾包里 `"usage"` 键被截断一半时应安全返回 0 而非 panic。
        let body = br#"{"usage":{"total_tokens""#;
        assert_eq!(
            extract_tokens(Protocol::Openai, Some("application/json"), body),
            0
        );
    }
}
