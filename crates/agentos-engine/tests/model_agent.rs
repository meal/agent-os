//! `ModelAgent`: deterministic Messages API requests behind the `Agent` seam.

mod common;

use agentos_core::ids::Digest;
use agentos_engine::agent::{Agent, AgentAction, ModelAgent, Observation};
use common::contract_model;
use serde_json::{json, Value};

fn agent() -> ModelAgent {
    ModelAgent::new(contract_model(12, 10).0, "claude-opus-5-5")
}

fn start() -> Observation {
    Observation::Start { files: vec!["src/parser.py".into(), "README.md".into()], workspace: Digest::of(b"ws") }
}

fn response_with(content: Value, stop_reason: &str) -> Observation {
    Observation::ModelResponse { content, stop_reason: stop_reason.into(), output_tokens: 7 }
}

fn response(tool: &str, input: Value) -> Observation {
    response_with(
        json!([
            {"type": "thinking", "thinking": "", "signature": "sig-1"},
            {"type": "tool_use", "id": "toolu_fake_1", "name": tool, "input": input}
        ]),
        "tool_use",
    )
}

fn body_of(a: &AgentAction) -> &[u8] {
    match a {
        AgentAction::CallModel { body, .. } => body,
        other => panic!("expected CallModel, got {other:?}"),
    }
}

fn last(a: &ModelAgent) -> Value {
    a.history().last().unwrap().clone()
}

#[test]
fn same_observations_give_identical_request_bytes_and_digest() {
    let mut a = agent();
    let mut b = agent();
    let steps = [
        start(),
        response("list_files", json!({})),
        Observation::Files { files: vec!["a".into(), "b".into()] },
        response("read_file", json!({"path": "src/parser.py"})),
    ];
    for obs in &steps {
        let (x, y) = (a.next(obs), b.next(obs));
        assert_eq!(x, y);
        if let AgentAction::CallModel { request, body } = &x {
            assert_eq!(body_of(&x), body_of(&y));
            assert_eq!(request, &Digest::of(body));
            assert!(!body.is_empty());
        }
    }
}

#[test]
fn request_bytes_have_sorted_keys_and_no_volatile_fields() {
    let mut a = agent();
    let act = a.next(&start());
    let body = body_of(&act).to_vec();
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(serde_json::to_vec(&v).unwrap(), body);
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(|k| k.as_str()).collect();
    assert_eq!(keys, ["max_tokens", "messages", "model", "system", "tool_choice", "tools"]);
    assert_eq!(v["tool_choice"], json!({"type": "auto", "disable_parallel_tool_use": true}));
    assert_eq!(v["max_tokens"], 1000);
    assert_eq!(v["model"], "claude-opus-5-5");
    assert_eq!(v["system"], agentos_engine::agent::SYSTEM_PROMPT);
    let tools = v["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["list_files", "read_file", "apply_patch", "run_verification", "finish"]);
    for t in tools {
        assert_eq!(t["strict"], true);
        assert_eq!(t["input_schema"]["additionalProperties"], false);
        assert_eq!(t["input_schema"]["type"], "object");
    }
    let text = String::from_utf8(body.clone()).unwrap();
    assert!(!text.contains("\"timestamp\"") && !text.contains("\"ts\""));
    // the first user message
    let msg = v["messages"][0]["content"].as_str().or_else(|| v["messages"][0]["content"][0]["text"].as_str()).unwrap();
    assert_eq!(
        msg,
        "Goal: fix the parser\n\nEditable paths: src/**\n\nFiles in the repository:\nsrc/parser.py\nREADME.md\n\nInspect the relevant files, fix the code with apply_patch, then run the verification."
    );
    // asked twice => identical bytes
    a.next(&response("list_files", json!({})));
    let first = a.next(&Observation::ModelCallLost);
    let second = a.next(&Observation::ModelCallLost);
    assert_eq!(first, second);
    assert_eq!(body_of(&first), body_of(&second));
}

#[test]
fn tool_use_blocks_map_to_actions_and_thinking_blocks_are_echoed_back() {
    let cases: Vec<(&str, Value, AgentAction)> = vec![
        ("list_files", json!({}), AgentAction::ListFiles),
        ("read_file", json!({"path": "src/a.py"}), AgentAction::ReadFile("src/a.py".into())),
        ("apply_patch", json!({"patch": "P"}), AgentAction::ApplyPatch("P".into())),
        ("run_verification", json!({}), AgentAction::Verify),
        ("finish", json!({"summary": "done"}), AgentAction::Finish),
    ];
    for (tool, input, want) in cases {
        let mut a = agent();
        a.next(&start());
        let obs = response(tool, input);
        let content = match &obs {
            Observation::ModelResponse { content, .. } => content.clone(),
            _ => unreachable!(),
        };
        assert_eq!(a.next(&obs), want, "{tool}");
        assert_eq!(last(&a), json!({"role": "assistant", "content": content}));
        assert_eq!(last(&a)["content"][0]["type"], "thinking");
    }
}

#[test]
fn tool_results_name_the_pending_tool_use_id_and_flag_errors() {
    let ws = Digest::of(b"w");
    let cases: Vec<(Observation, bool, Option<&str>)> = vec![
        (Observation::Files { files: vec!["a".into(), "b".into()] }, false, Some("a\nb")),
        (Observation::PatchRejected { reason: "nope".into() }, true, Some("patch rejected: nope")),
        (Observation::Verification { passed: false, summary: "s".into() }, true, Some("verification failed: s")),
        (Observation::Verification { passed: true, summary: "s".into() }, false, Some("verification passed: s")),
        (Observation::FileReadRejected { reason: "r".into() }, true, Some("r")),
        (Observation::PatchApplied { workspace: ws }, false, None),
        (
            Observation::VersionConflict { expected: ws, actual: ws },
            true,
            None,
        ),
    ];
    for (obs, is_err, text) in cases {
        let mut a = agent();
        a.next(&start());
        a.next(&response("list_files", json!({})));
        assert!(matches!(a.next(&obs), AgentAction::CallModel { .. }), "{obs:?}");
        let m = last(&a);
        assert_eq!(m["role"], "user");
        let block = &m["content"][0];
        assert_eq!(block["type"], "tool_result");
        assert_eq!(block["tool_use_id"], "toolu_fake_1");
        if is_err {
            assert_eq!(block["is_error"], true, "{obs:?}");
        } else {
            assert!(block.get("is_error").is_none(), "{obs:?}");
        }
        if let Some(t) = text {
            assert_eq!(block["content"], t);
        }
    }
    let mut a = agent();
    a.next(&start());
    a.next(&response("read_file", json!({"path": "p"})));
    a.next(&Observation::FileRead { path: "p".into(), content: "abc".into(), truncated: true });
    assert_eq!(last(&a)["content"][0]["content"], "abc\n[truncated at 65536 bytes]");
    let mut a = agent();
    a.next(&start());
    a.next(&response("read_file", json!({"path": "p"})));
    a.next(&Observation::FileRead { path: "p".into(), content: "abc".into(), truncated: false });
    assert_eq!(last(&a)["content"][0]["content"], "abc");
}

#[test]
fn stop_without_a_tool_use_is_finish() {
    for reason in ["end_turn", "max_tokens", "refusal", ""] {
        let mut a = agent();
        a.next(&start());
        let obs = response_with(json!([{"type": "text", "text": "hello"}]), reason);
        assert_eq!(a.next(&obs), AgentAction::Finish, "{reason:?}");
    }
}

#[test]
fn a_tool_use_without_a_string_id_is_finish() {
    let mut a = agent();
    a.next(&start());
    let obs = response_with(json!([{"type": "tool_use", "id": 5, "name": "list_files", "input": {}}]), "tool_use");
    assert_eq!(a.next(&obs), AgentAction::Finish);
}

#[test]
fn a_malformed_or_unknown_tool_input_is_answered_as_an_error_and_asked_again() {
    for (tool, input, text) in [
        ("read_file", json!({"path": 5}), "invalid tool input for read_file"),
        ("apply_patch", json!({}), "invalid tool input for apply_patch"),
        ("teleport", json!({}), "unknown tool teleport"),
    ] {
        let mut a = agent();
        a.next(&start());
        assert!(matches!(a.next(&response(tool, input)), AgentAction::CallModel { .. }));
        let block = &last(&a)["content"][0];
        assert_eq!(block["type"], "tool_result");
        assert_eq!(block["is_error"], true);
        assert_eq!(block["tool_use_id"], "toolu_fake_1");
        assert_eq!(block["content"], text);
    }
}

#[test]
fn failed_and_lost_calls_are_asked_again_and_budget_exhaustion_finishes() {
    let mut a = agent();
    let first = a.next(&start());
    let n = a.history().len();
    let again = a.next(&Observation::ModelCallFailed { reason: "http 500".into(), failure: None });
    assert_eq!(body_of(&first), body_of(&again));
    let lost = a.next(&Observation::ModelCallLost);
    assert_eq!(body_of(&first), body_of(&lost));
    assert_eq!(a.history().len(), n);
    assert_eq!(a.next(&Observation::BudgetExhausted), AgentAction::Finish);
}

#[test]
fn a_tool_observation_without_a_pending_call_is_finish() {
    let mut a = agent();
    a.next(&start());
    assert_eq!(a.next(&Observation::Files { files: vec![] }), AgentAction::Finish);
    assert_eq!(a.next(&Observation::Verification { passed: true, summary: "x".into() }), AgentAction::Finish);
}

#[test]
fn the_fixture_transcripts_drive_the_documented_action_sequence() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/transcripts/parser-fix.json");
    let t: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let responses: Vec<Observation> = t["responses"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| Observation::ModelResponse {
            content: r["response"]["content"].clone(),
            stop_reason: r["response"]["stop_reason"].as_str().unwrap().into(),
            output_tokens: 20,
        })
        .collect();
    assert_eq!(responses.len(), 6);
    let ws = Digest::of(b"w");
    let tool_obs = [
        Observation::Files { files: vec!["src/parser.py".into()] },
        Observation::FileRead { path: "src/parser.py".into(), content: "x".into(), truncated: false },
        Observation::PatchApplied { workspace: ws },
        Observation::Verification { passed: false, summary: "fail".into() },
        Observation::PatchApplied { workspace: ws },
        Observation::Verification { passed: true, summary: "ok".into() },
    ];
    let mut a = agent();
    let mut actions = vec![a.next(&start())];
    for (r, o) in responses.iter().zip(tool_obs.iter()) {
        actions.push(a.next(r));
        actions.push(a.next(o));
    }
    let kinds: Vec<String> = actions
        .iter()
        .map(|x| match x {
            AgentAction::CallModel { .. } => "CallModel".to_string(),
            AgentAction::ListFiles => "ListFiles".into(),
            AgentAction::ReadFile(p) => format!("ReadFile({p})"),
            AgentAction::ApplyPatch(p) => format!("ApplyPatch({})", p.contains("notes.txt")),
            AgentAction::Verify => "Verify".into(),
            AgentAction::Finish => "Finish".into(),
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "CallModel", "ListFiles", "CallModel", "ReadFile(src/parser.py)", "CallModel", "ApplyPatch(true)",
            "CallModel", "Verify", "CallModel", "ApplyPatch(false)", "CallModel", "Verify", "CallModel"
        ]
    );
}

#[test]
fn a_response_with_several_tool_uses_is_finish_and_sets_no_pending_call() {
    let mut a = agent();
    a.next(&start());
    let content = json!([
        {"type": "thinking", "thinking": "", "signature": "sig-1"},
        {"type": "tool_use", "id": "toolu_a", "name": "list_files", "input": {}},
        {"type": "tool_use", "id": "toolu_b", "name": "apply_patch", "input": {"patch": "P"}}
    ]);
    assert_eq!(a.next(&response_with(content.clone(), "tool_use")), AgentAction::Finish);
    assert_eq!(last(&a), json!({"role": "assistant", "content": content}));
    // no pending id: a stray tool observation is Finish, not a result for toolu_a
    assert_eq!(a.next(&Observation::Files { files: vec![] }), AgentAction::Finish);
}

#[test]
fn one_tool_use_among_text_and_thinking_blocks_still_maps_normally() {
    let mut a = agent();
    a.next(&start());
    let content = json!([
        {"type": "thinking", "thinking": "", "signature": "s"},
        {"type": "text", "text": "ok"},
        {"type": "tool_use", "id": "toolu_x", "name": "run_verification", "input": {}},
        {"type": "text", "text": "tail"}
    ]);
    assert_eq!(a.next(&response_with(content, "tool_use")), AgentAction::Verify);
}

/// What the journal does to an observation between the live run and a replay.
fn journaled(obs: &Observation) -> Observation {
    serde_json::from_slice(&serde_json::to_vec(obs).unwrap()).unwrap()
}

#[test]
fn replaying_the_journal_round_trip_gives_the_live_request_digests() {
    // Floats anywhere in a response (thinking, text, a tool input, usage-like fields) must
    // survive the journal's JSON round trip exactly, or the replayed request differs.
    let floats = [0.1, 0.30000000000000004, 1.0 / 3.0, 2.2250738585072014e-308, 1.7976931348623157e308, 123456789.12345679, 5e-324, 0.1 + 0.7];
    let live_response = |tool: &str, input: Value| {
        // Parsed from text, as a retained provider response is, not built from f64 literals.
        let text = format!(
            r#"[{{"type":"thinking","thinking":"t","signature":"s","score":{}}},{{"type":"text","text":"x","weights":[{}]}},{{"type":"tool_use","id":"toolu_1","name":"{tool}","input":{}}}]"#,
            "0.12345678901234567890123", "0.30000000000000004123, 1e-7, 4.35, 2.9999999999999996", input
        );
        response_with(serde_json::from_str(&text).unwrap(), "tool_use")
    };
    let mut observations = vec![start(), live_response("list_files", json!({}))];
    for f in floats {
        observations.push(Observation::Files { files: vec![format!("{f}")] });
        observations.push(live_response("read_file", json!({"path": "src/parser.py", "ratio": f})));
    }

    let mut live = agent();
    let mut replay = agent();
    for obs in &observations {
        let (x, y) = (live.next(obs), replay.next(&journaled(obs)));
        if let (AgentAction::CallModel { request: a, body: b1 }, AgentAction::CallModel { request: b, body: b2 }) = (&x, &y) {
            assert_eq!(a, b, "the digest of the request survives the journal");
            assert_eq!(b1, b2);
        } else {
            assert_eq!(x, y);
        }
    }
}

#[test]
fn many_long_floats_survive_the_journal_round_trip() {
    // Provider text carries floats with up to 17+ significant digits; serde_json without
    // `float_roundtrip` can parse such text one ulp off. The pin: parse, journal, parse
    // again must be a fixed point, for a deterministic spread of doubles.
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    for _ in 0..50_000 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let f = f64::from_bits(x);
        if !f.is_finite() {
            continue;
        }
        for text in [format!("{f:.20e}"), format!("{f:.25e}"), format!("{}", (x >> 11) as f64 / (1u64 << 53) as f64)] {
            let live: Value = serde_json::from_str(&format!("[{text}]")).unwrap();
            let replay: Value = serde_json::from_slice(&serde_json::to_vec(&live).unwrap()).unwrap();
            assert_eq!(live, replay, "{text}");
            assert_eq!(serde_json::to_vec(&live).unwrap(), serde_json::to_vec(&replay).unwrap(), "{text}");
        }
    }
}
