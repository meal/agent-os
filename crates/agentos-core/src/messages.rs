//! Codec for the Messages API traffic between the guest model proxy and the host: request
//! normalization before a call is made, and SSE framing of a whole response for a streaming
//! client.

use serde_json::{Value, json};

/// Request fields the forwarded body must not carry: beta-only, and no beta header is sent.
pub const STRIPPED_FIELDS: [&str; 3] = ["context_management", "safeguards", "output_config"];

/// Canonical forwarded body: `stream` is false, `max_tokens` is at most `max_tokens_cap`,
/// the stripped fields are gone, and keys are sorted by serde_json.
pub fn normalize_request(body: &[u8], max_tokens_cap: u32) -> Result<Vec<u8>, String> {
    let mut request: Value =
        serde_json::from_slice(body).map_err(|e| format!("request body is not JSON: {e}"))?;
    let object = request
        .as_object_mut()
        .ok_or("request body is not a JSON object")?;
    if !object.get("messages").is_some_and(Value::is_array) {
        return Err("request body has no messages array".into());
    }
    let cap = u64::from(max_tokens_cap);
    let max_tokens = object
        .get("max_tokens")
        .and_then(Value::as_u64)
        .map_or(cap, |requested| requested.min(cap));
    object.insert("max_tokens".into(), Value::from(max_tokens));
    object.insert("stream".into(), Value::Bool(false));
    for field in STRIPPED_FIELDS {
        object.remove(field);
    }
    if let Some(messages) = object.get_mut("messages").and_then(Value::as_array_mut) {
        *messages = plain_messages(messages)?;
    }
    serde_json::to_vec(&request).map_err(|e| e.to_string())
}

/// The messages as the standard Messages API takes them: only `role` and `content`, and no
/// `system` role (a beta feature, sent without its beta header). A `system` message's content
/// joins the `user` message before it, or else the one after it, after that message's own
/// blocks, so a `tool_result` is never separated from its `tool_use` and stays first; a
/// `system` message with no `user` message to join stands alone as one.
fn plain_messages(messages: &[Value]) -> Result<Vec<Value>, String> {
    let mut out: Vec<Value> = Vec::new();
    let mut pending: Vec<Value> = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        let object = message
            .as_object()
            .ok_or_else(|| format!("message {index} is not an object"))?;
        let role = object
            .get("role")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("message {index} has no role"))?;
        let content = object.get("content").cloned().unwrap_or(Value::Null);
        match role {
            "system" => {
                let blocks = blocks_of(content);
                match out.last_mut() {
                    Some(last) if last["role"] == "user" => append_blocks(last, blocks),
                    _ => pending.extend(blocks),
                }
            }
            role => {
                let mut plain = json!({"role": role, "content": content});
                if role == "user" && !pending.is_empty() {
                    append_blocks(&mut plain, std::mem::take(&mut pending));
                }
                out.push(plain);
            }
        }
    }
    if !pending.is_empty() {
        out.push(json!({"role": "user", "content": pending}));
    }
    Ok(out)
}

/// Content as a list of blocks (a bare string is one text block).
fn blocks_of(content: Value) -> Vec<Value> {
    match content {
        Value::Array(blocks) => blocks,
        Value::String(text) => vec![json!({"type": "text", "text": text})],
        _ => Vec::new(),
    }
}

fn append_blocks(message: &mut Value, extra: Vec<Value>) {
    let mut blocks = blocks_of(message["content"].take());
    blocks.extend(extra);
    message["content"] = Value::Array(blocks);
}

/// True only when the body is a JSON object whose `stream` is `true`.
pub fn wants_stream(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|request| request.get("stream").and_then(Value::as_bool))
        == Some(true)
}

/// A Messages SSE stream that replays a whole response.
pub fn sse_from_message(response: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    let usage = response.get("usage");
    let message = json!({
        "id": response.get("id").cloned().unwrap_or(Value::Null),
        "type": response.get("type").cloned().unwrap_or(Value::Null),
        "role": response.get("role").cloned().unwrap_or(Value::Null),
        "model": response.get("model").cloned().unwrap_or(Value::Null),
        "content": [],
        "stop_reason": null,
        "stop_sequence": null,
        "usage": {
            "input_tokens": usage.and_then(|u| u.get("input_tokens")).cloned().unwrap_or(json!(0)),
            "output_tokens": 0
        }
    });
    push_event(
        &mut out,
        "message_start",
        json!({"type": "message_start", "message": message}),
    );
    let blocks = response
        .get("content")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    for (index, block) in blocks.iter().enumerate() {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                start_block(&mut out, index, json!({"type": "text", "text": ""}));
                let delta = json!({"type": "text_delta", "text": text});
                push_delta(&mut out, index, delta);
            }
            Some("tool_use") => {
                let mut start = block.clone();
                start["input"] = json!({});
                start_block(&mut out, index, start);
                let input = block.get("input").cloned().unwrap_or(json!({}));
                let partial_json = serde_json::to_string(&input).unwrap_or_default();
                let delta = json!({"type": "input_json_delta", "partial_json": partial_json});
                push_delta(&mut out, index, delta);
            }
            Some("thinking") => {
                // A stream starts the block empty and delivers the text and then the signature
                // in deltas; the client rebuilds the block from them and sends it back, and the
                // API refuses a thinking block that lost its signature.
                let text = block.get("thinking").and_then(Value::as_str).unwrap_or("");
                let signature = block.get("signature").and_then(Value::as_str).unwrap_or("");
                start_block(
                    &mut out,
                    index,
                    json!({"type": "thinking", "thinking": "", "signature": ""}),
                );
                if !text.is_empty() {
                    let delta = json!({"type": "thinking_delta", "thinking": text});
                    push_delta(&mut out, index, delta);
                }
                if !signature.is_empty() {
                    let delta = json!({"type": "signature_delta", "signature": signature});
                    push_delta(&mut out, index, delta);
                }
            }
            _ => start_block(&mut out, index, block.clone()),
        }
        push_event(
            &mut out,
            "content_block_stop",
            json!({"type": "content_block_stop", "index": index}),
        );
    }
    push_event(
        &mut out,
        "message_delta",
        json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": response.get("stop_reason").cloned().unwrap_or(Value::Null),
                "stop_sequence": response.get("stop_sequence").cloned().unwrap_or(Value::Null)
            },
            "usage": {
                "output_tokens": usage.and_then(|u| u.get("output_tokens")).cloned().unwrap_or(json!(0))
            }
        }),
    );
    push_event(&mut out, "message_stop", json!({"type": "message_stop"}));
    out
}

fn start_block(out: &mut Vec<u8>, index: usize, content_block: Value) {
    push_event(
        out,
        "content_block_start",
        json!({"type": "content_block_start", "index": index, "content_block": content_block}),
    );
}

fn push_delta(out: &mut Vec<u8>, index: usize, delta: Value) {
    push_event(
        out,
        "content_block_delta",
        json!({"type": "content_block_delta", "index": index, "delta": delta}),
    );
}

fn push_event(out: &mut Vec<u8>, name: &str, data: Value) {
    out.extend_from_slice(format!("event: {name}\ndata: {data}\n\n").as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAP: u32 = 8192;

    fn normalized(body: &str) -> Value {
        serde_json::from_slice(&normalize_request(body.as_bytes(), CAP).unwrap()).unwrap()
    }

    /// Parses an SSE stream into (event, data) pairs, checking the framing on the way.
    fn events(sse: &[u8]) -> Vec<(String, Value)> {
        let text = std::str::from_utf8(sse).unwrap();
        assert!(text.ends_with("\n\n"), "stream must end with a blank line");
        text.split_terminator("\n\n")
            .map(|event| {
                let lines: Vec<&str> = event.split('\n').collect();
                assert_eq!(lines.len(), 2, "event is two lines: {event:?}");
                let name = lines[0].strip_prefix("event: ").expect("event line");
                let data = lines[1].strip_prefix("data: ").expect("data line");
                let value: Value = serde_json::from_str(data).unwrap();
                assert_eq!(value["type"], name);
                (name.to_string(), value)
            })
            .collect()
    }

    #[test]
    fn max_tokens_above_the_cap_is_clamped() {
        let body = normalized(r#"{"messages":[],"max_tokens":128000}"#);
        assert_eq!(body["max_tokens"], json!(CAP));
    }

    #[test]
    fn missing_max_tokens_becomes_the_cap() {
        let body = normalized(r#"{"messages":[]}"#);
        assert_eq!(body["max_tokens"], json!(CAP));
    }

    #[test]
    fn smaller_max_tokens_is_kept() {
        let body = normalized(r#"{"messages":[],"max_tokens":100}"#);
        assert_eq!(body["max_tokens"], json!(100));
    }

    #[test]
    fn stream_true_is_forced_false() {
        let body = normalized(r#"{"messages":[],"stream":true}"#);
        assert_eq!(body["stream"], json!(false));
    }

    #[test]
    fn a_mid_conversation_system_message_joins_the_user_message_before_it() {
        // What the live API refused: `messages.1.output_config: Extra inputs are not permitted`
        // on a `system`-role message the agent CLI sends with its own `output_config`.
        let body = r#"{"messages":[
            {"role":"user","content":[{"type":"text","text":"goal"}]},
            {"role":"system","output_config":{"effort":"low"},"content":[{"type":"text","text":"reminder","cache_control":{"type":"ephemeral"}}]},
            {"role":"assistant","content":"ok","extra":1}
        ]}"#;
        let request = normalized(body);
        let messages = request["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(
            messages[0]["content"][1]["cache_control"]["type"],
            "ephemeral"
        );
        for message in messages {
            let mut keys: Vec<_> = message.as_object().unwrap().keys().cloned().collect();
            keys.sort();
            assert_eq!(keys, ["content", "role"], "{message}");
        }
        assert_eq!(messages[0]["content"][0]["text"], "goal");
        assert_eq!(messages[1]["role"], "assistant");
    }

    #[test]
    fn a_system_message_between_a_tool_use_and_its_result_never_separates_them() {
        let body = r#"{"messages":[
            {"role":"user","content":"goal"},
            {"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{}}]},
            {"role":"system","content":[{"type":"text","text":"reminder"}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]},
            {"role":"system","content":"trailing"}
        ]}"#;
        let request = normalized(body);
        let messages = request["messages"].as_array().unwrap();
        let roles: Vec<_> = messages
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, ["user", "assistant", "user"]);
        let last = messages[2]["content"].as_array().unwrap();
        assert_eq!(last[0]["type"], "tool_result", "{last:?}");
        assert_eq!(last[1], json!({"type":"text","text":"reminder"}));
        assert_eq!(last[2], json!({"type":"text","text":"trailing"}));
    }

    #[test]
    fn a_message_that_is_not_an_object_is_refused() {
        let err = normalize_request(br#"{"messages":["plain"]}"#, CAP).unwrap_err();
        assert!(err.contains("message 0"), "{err}");
    }

    #[test]
    fn stripped_fields_are_removed_and_others_kept() {
        let body = normalized(
            r#"{"messages":[{"role":"user","content":"hi"}],"model":"m","system":"s",
                "tools":[],"thinking":{"type":"enabled"},"metadata":{"user_id":"u"},
                "context_management":{},"safeguards":{},"output_config":{}}"#,
        );
        for field in STRIPPED_FIELDS {
            assert!(body.get(field).is_none(), "{field} must be removed");
        }
        for field in [
            "messages", "model", "system", "tools", "thinking", "metadata",
        ] {
            assert!(body.get(field).is_some(), "{field} must be kept");
        }
    }

    #[test]
    fn non_object_body_is_rejected() {
        assert!(normalize_request(b"[1,2]", CAP).is_err());
        assert!(normalize_request(b"\"text\"", CAP).is_err());
        assert!(normalize_request(b"not json", CAP).is_err());
    }

    #[test]
    fn body_without_messages_array_is_rejected() {
        assert!(normalize_request(br#"{"model":"m"}"#, CAP).is_err());
        assert!(normalize_request(br#"{"messages":"hi"}"#, CAP).is_err());
    }

    #[test]
    fn output_is_byte_identical_for_two_key_orders() {
        let a = br#"{"messages":[],"model":"m","max_tokens":5,"system":"s","stream":true}"#;
        let b = br#"{"stream":true,"system":"s","max_tokens":5,"model":"m","messages":[]}"#;
        assert_eq!(
            normalize_request(a, CAP).unwrap(),
            normalize_request(b, CAP).unwrap()
        );
    }

    #[test]
    fn wants_stream_reads_only_a_true_stream_flag() {
        assert!(wants_stream(br#"{"stream":true}"#));
        assert!(!wants_stream(br#"{"stream":false}"#));
        assert!(!wants_stream(br#"{"messages":[]}"#));
        assert!(!wants_stream(br#"{"stream":"true"}"#));
        assert!(!wants_stream(b"garbage"));
        assert!(!wants_stream(b""));
    }

    fn response(content: Value, stop_reason: &str) -> Value {
        json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "claude-opus-5-5",
            "content": content,
            "stop_reason": stop_reason,
            "stop_sequence": null,
            "usage": {"input_tokens": 12, "output_tokens": 34}
        })
    }

    #[test]
    fn sse_for_a_text_block() {
        let sse = sse_from_message(&response(
            json!([{"type": "text", "text": "hello"}]),
            "end_turn",
        ));
        let events = events(&sse);
        let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        let start = &events[0].1["message"];
        assert_eq!(start["id"], "msg_1");
        assert_eq!(start["content"], json!([]));
        assert_eq!(start["stop_reason"], Value::Null);
        assert_eq!(
            start["usage"],
            json!({"input_tokens": 12, "output_tokens": 0})
        );
        assert_eq!(
            events[1].1["content_block"],
            json!({"type": "text", "text": ""})
        );
        assert_eq!(events[2].1["index"], json!(0));
        assert_eq!(
            events[2].1["delta"],
            json!({"type": "text_delta", "text": "hello"})
        );
        assert_eq!(events[4].1["delta"]["stop_reason"], "end_turn");
        assert_eq!(events[4].1["usage"], json!({"output_tokens": 34}));
    }

    #[test]
    fn sse_for_a_tool_use_block_round_trips_its_input() {
        let input = json!({"path": "a.txt", "nested": {"n": [1, 2]}});
        let sse = sse_from_message(&response(
            json!([{"type": "tool_use", "id": "t1", "name": "edit", "input": input}]),
            "tool_use",
        ));
        let events = events(&sse);
        assert_eq!(events[1].0, "content_block_start");
        assert_eq!(events[1].1["content_block"]["input"], json!({}));
        assert_eq!(events[1].1["content_block"]["name"], "edit");
        assert_eq!(events[2].1["delta"]["type"], "input_json_delta");
        let partial = events[2].1["delta"]["partial_json"].as_str().unwrap();
        assert_eq!(serde_json::from_str::<Value>(partial).unwrap(), input);
        assert_eq!(events[4].1["delta"]["stop_reason"], "tool_use");
    }

    #[test]
    fn sse_for_a_thinking_block_carries_its_signature() {
        // What the live API sends with `display: omitted`: no thinking text, a signature that
        // must come back with the block (a stream delivers it as a `signature_delta`).
        let sse = sse_from_message(&response(
            json!([
                {"type": "thinking", "thinking": "", "signature": "sig-abc"},
                {"type": "thinking", "thinking": "step one", "signature": "sig-def"},
                {"type": "redacted_thinking", "data": "opaque"}
            ]),
            "end_turn",
        ));
        let events = events(&sse);
        let names: Vec<_> = events.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        assert_eq!(
            events[1].1["content_block"],
            json!({"type": "thinking", "thinking": "", "signature": ""})
        );
        assert_eq!(
            events[2].1["delta"],
            json!({"type": "signature_delta", "signature": "sig-abc"})
        );
        assert_eq!(
            events[5].1["delta"],
            json!({"type": "thinking_delta", "thinking": "step one"})
        );
        assert_eq!(
            events[6].1["delta"],
            json!({"type": "signature_delta", "signature": "sig-def"})
        );
        assert_eq!(
            events[8].1["content_block"],
            json!({"type": "redacted_thinking", "data": "opaque"})
        );
    }

    #[test]
    fn sse_for_empty_content_has_no_block_events() {
        let sse = sse_from_message(&response(json!([]), "end_turn"));
        let names: Vec<String> = events(&sse).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["message_start", "message_delta", "message_stop"]);
    }

    #[test]
    fn sse_events_are_indexed_in_order_and_each_ends_with_a_blank_line() {
        let sse = sse_from_message(&response(
            json!([
                {"type": "text", "text": "a"},
                {"type": "tool_use", "id": "t", "name": "n", "input": {}},
                {"type": "text", "text": "b"}
            ]),
            "end_turn",
        ));
        let events = events(&sse);
        let indexes: Vec<u64> = events
            .iter()
            .filter(|(n, _)| n == "content_block_start")
            .map(|(_, v)| v["index"].as_u64().unwrap())
            .collect();
        assert_eq!(indexes, [0, 1, 2]);
        let stops: Vec<u64> = events
            .iter()
            .filter(|(n, _)| n == "content_block_stop")
            .map(|(_, v)| v["index"].as_u64().unwrap())
            .collect();
        assert_eq!(stops, [0, 1, 2]);
        assert_eq!(events.first().unwrap().0, "message_start");
        assert_eq!(events.last().unwrap().0, "message_stop");
        for chunk in std::str::from_utf8(&sse).unwrap().split("\n\n") {
            assert!(!chunk.contains("\n\n"));
        }
    }
}
