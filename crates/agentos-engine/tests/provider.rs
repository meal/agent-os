mod common;

use std::time::{Duration, Instant};

use agentos_engine::model::anthropic::{AnthropicProvider, PROVIDER_TEXT_LIMIT};
use agentos_engine::model::fake::load_transcript;
use agentos_engine::model::provider::{ApiKey, ModelProvider, ProviderResult, Usage};
use common::http::{FakeApi, Reply, serve};
use common::{fix_patch, transcript};

const BODY: &[u8] = br#"{"messages":[{"role":"user","content":"go"}]}"#;

fn provider(api: &FakeApi, timeout: Duration) -> AnthropicProvider {
    AnthropicProvider::new(ApiKey::new("sk-ant-test-SECRET").unwrap()).with_base_url(api.url()).with_timeout(timeout)
}

fn secs2() -> Duration {
    Duration::from_secs(2)
}

#[tokio::test]
async fn a_2xx_is_a_response_with_its_usage() {
    let api = serve(Reply::Transcript(transcript("parser-fix-direct")));
    match provider(&api, secs2()).complete(BODY).await {
        ProviderResult::Response(bytes, usage) => {
            assert_eq!(usage, Usage { input_tokens: 100, output_tokens: 20 });
            let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(v["content"][1]["name"], "list_files");
        }
        other => panic!("{other:?}"),
    }
    let reqs = api.requests();
    assert_eq!(reqs.len(), 1);
    let header = |k: &str| reqs[0].headers.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    assert_eq!(header("x-api-key").as_deref(), Some("sk-ant-test-SECRET"));
    assert_eq!(header("anthropic-version").as_deref(), Some("2023-06-01"));
    assert_eq!(header("content-type").as_deref(), Some("application/json"));
    assert_eq!(reqs[0].body, BODY);
    assert_eq!(api.hits(), 1);
}

#[tokio::test]
async fn a_4xx_is_rejected_with_its_status_and_body_and_one_send() {
    let text = r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad"}}"#;
    let api = serve(Reply::Status(400, text.into()));
    assert_eq!(
        provider(&api, secs2()).complete(BODY).await,
        ProviderResult::Rejected { status: 400, body: text.into() }
    );
    assert_eq!(api.hits(), 1);
}

#[tokio::test]
async fn a_5xx_and_429_are_rejected_not_retried() {
    for status in [500u16, 503, 429] {
        let api = serve(Reply::Status(status, "{}".into()));
        let r = provider(&api, secs2()).complete(BODY).await;
        assert!(matches!(r, ProviderResult::Rejected { status: s, .. } if s == status), "{r:?}");
        assert_eq!(api.hits(), 1, "status {status}");
    }
}

#[tokio::test]
async fn a_timeout_is_a_transport_failure_after_exactly_one_send() {
    let api = serve(Reply::Hang(Duration::from_secs(5)));
    let t = Instant::now();
    let r = provider(&api, Duration::from_millis(300)).complete(BODY).await;
    assert!(matches!(r, ProviderResult::Transport(_)), "{r:?}");
    assert!(t.elapsed() < Duration::from_millis(1500), "took {:?}", t.elapsed());
    assert_eq!(api.hits(), 1);
}

#[tokio::test]
async fn a_connection_cut_mid_body_is_a_transport_failure() {
    let api = serve(Reply::CloseMidBody);
    let r = provider(&api, secs2()).complete(BODY).await;
    assert!(matches!(r, ProviderResult::Transport(_)), "{r:?}");
    assert_eq!(api.hits(), 1);
}

fn closed_port_url() -> String {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    format!("http://127.0.0.1:{port}")
}

#[tokio::test]
async fn a_refused_connection_is_a_transport_failure() {
    let p = AnthropicProvider::new(ApiKey::new("sk-ant-test-SECRET").unwrap())
        .with_base_url(closed_port_url())
        .with_timeout(secs2());
    assert!(matches!(p.complete(BODY).await, ProviderResult::Transport(_)));
}

#[tokio::test]
async fn provider_errors_never_quote_the_key() {
    let mut results = Vec::new();
    let mut debugs = Vec::new();
    for reply in [
        Reply::Transcript(transcript("parser-fix-direct")),
        Reply::Status(400, "{}".into()),
        Reply::Status(500, "{}".into()),
        Reply::Hang(Duration::from_secs(2)),
        Reply::CloseMidBody,
    ] {
        let api = serve(reply);
        let p = provider(&api, Duration::from_millis(300));
        results.push(p.complete(BODY).await);
        debugs.push(format!("{p:?}"));
    }
    let refused = AnthropicProvider::new(ApiKey::new("sk-ant-test-SECRET").unwrap())
        .with_base_url(closed_port_url())
        .with_timeout(secs2());
    results.push(refused.complete(BODY).await);
    debugs.push(format!("{refused:?}"));
    for r in &results {
        assert!(!format!("{r:?}").contains("SECRET"), "{r:?}");
        if let ProviderResult::Transport(t) = r {
            assert!(!t.contains("SECRET"), "{t}");
        }
    }
    assert!(results.iter().filter(|r| matches!(r, ProviderResult::Transport(_))).count() >= 3);
    for d in debugs {
        assert!(!d.contains("SECRET"), "{d}");
    }
}

#[tokio::test]
async fn provider_error_bodies_are_bounded_before_they_reach_the_journal() {
    let api = serve(Reply::Status(400, "x".repeat(100_000)));
    match provider(&api, secs2()).complete(BODY).await {
        ProviderResult::Rejected { body, .. } => assert_eq!(body.len(), PROVIDER_TEXT_LIMIT),
        other => panic!("{other:?}"),
    }
}

fn tools(name: &str) -> Vec<serde_json::Value> {
    load_transcript(&transcript(name)).unwrap().responses.into_iter().map(|e| e.response["content"][1].clone()).collect()
}

#[test]
fn the_fixture_transcripts_parse_and_have_the_documented_tool_sequence() {
    let full = tools("parser-fix");
    let names: Vec<_> = full.iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
    assert_eq!(names, ["list_files", "read_file", "apply_patch", "run_verification", "apply_patch", "run_verification"]);
    assert_eq!(full[4]["input"]["patch"], fix_patch());
    assert_eq!(full[1]["input"]["path"], "src/parser.py");
    let direct = tools("parser-fix-direct");
    let names: Vec<_> = direct.iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
    assert_eq!(names, ["list_files", "read_file", "apply_patch", "run_verification"]);
    assert_eq!(direct[2]["input"]["patch"], fix_patch());
}

#[tokio::test]
async fn a_redirect_is_rejected_never_followed() {
    for status in [301u16, 307] {
        let target = serve(Reply::Status(200, "{}".into()));
        let api = serve(Reply::Redirect(status, format!("{}/v1/messages", target.url())));
        let r = provider(&api, secs2()).complete(BODY).await;
        assert!(matches!(r, ProviderResult::Rejected { status: s, .. } if s == status), "{status}: {r:?}");
        assert_eq!(api.hits(), 1, "{status}");
        assert_eq!(target.hits(), 0, "{status}: the redirect target was contacted");
        assert!(target.requests().iter().all(|q| q.headers.iter().all(|(k, _)| k != "x-api-key")));
    }
}

#[tokio::test]
async fn a_connection_closed_before_any_bytes_is_one_send_and_a_transport_failure() {
    let api = serve(Reply::CloseAtOnce);
    let r = provider(&api, secs2()).complete(BODY).await;
    assert!(matches!(r, ProviderResult::Transport(_)), "{r:?}");
    assert_eq!(api.hits(), 1);
}
