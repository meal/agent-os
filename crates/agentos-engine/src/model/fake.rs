use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agentos_core::ids::Digest;
use serde::{Deserialize, Serialize};

use super::provider::{BoxFuture, ModelProvider, ProviderResult, usage_of};
use crate::job::atomic_write;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect_request_digest: Option<Digest>,
    pub response: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Transcript {
    pub responses: Vec<TranscriptEntry>,
}

pub fn load_transcript(path: &Path) -> io::Result<Transcript> {
    let bytes = std::fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", path.display())))
}

enum Mode {
    Transcript(Transcript),
    Scripted(Mutex<VecDeque<ProviderResult>>),
}

/// A provider that never touches the network: answers from a transcript by conversation
/// depth, or from a position-keyed script.
#[derive(Clone)]
pub struct FakeProvider {
    mode: Arc<Mode>,
    calls: Arc<AtomicUsize>,
}

impl FakeProvider {
    pub fn from_file(path: &Path) -> io::Result<FakeProvider> {
        Ok(FakeProvider::from_transcript(load_transcript(path)?))
    }

    pub fn from_transcript(t: Transcript) -> FakeProvider {
        FakeProvider { mode: Arc::new(Mode::Transcript(t)), calls: Arc::new(AtomicUsize::new(0)) }
    }

    pub fn scripted(results: Vec<ProviderResult>) -> FakeProvider {
        FakeProvider { mode: Arc::new(Mode::Scripted(Mutex::new(results.into()))), calls: Arc::new(AtomicUsize::new(0)) }
    }

    /// Another handle on the same script and call counter, for a test that hands the provider
    /// to an executor and keeps watching `calls()`.
    pub fn clone_handle(&self) -> FakeProvider {
        self.clone()
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    pub fn calls_counter(&self) -> Arc<AtomicUsize> {
        self.calls.clone()
    }
}

fn reject(body: String) -> ProviderResult {
    ProviderResult::Rejected { status: 400, body }
}

fn answer(t: &Transcript, body: &[u8]) -> ProviderResult {
    let n = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["messages"].as_array().map(|m| m.len()))
        .unwrap_or(0);
    if n == 0 || n.is_multiple_of(2) {
        return reject(format!("transcript: request has {n} messages"));
    }
    let i = (n - 1) / 2;
    let Some(entry) = t.responses.get(i) else {
        return reject(format!("transcript exhausted at depth {n}"));
    };
    if let Some(d) = entry.expect_request_digest {
        let x = Digest::of(body);
        if d != x {
            return reject(format!("transcript entry {i} expects request {d}, got {x}"));
        }
    }
    let bytes = serde_json::to_vec(&entry.response).expect("a JSON value serializes");
    ProviderResult::Response(bytes, usage_of(&entry.response))
}

impl ModelProvider for FakeProvider {
    fn complete<'a>(&'a self, body: &'a [u8]) -> BoxFuture<'a, ProviderResult> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match &*self.mode {
                Mode::Transcript(t) => answer(t, body),
                Mode::Scripted(q) => q
                    .lock()
                    .expect("script lock")
                    .pop_front()
                    .unwrap_or_else(|| reject("scripted provider exhausted".into())),
            }
        })
    }
}

/// Wraps a provider and writes every `Response` it sees to `path` as a [`Transcript`]
/// (request digest pinned, request bytes never stored).
pub struct Recording<P: ModelProvider> {
    inner: P,
    path: PathBuf,
    entries: Mutex<Vec<TranscriptEntry>>,
}

impl<P: ModelProvider> Recording<P> {
    pub fn new(inner: P, path: PathBuf) -> Recording<P> {
        Recording { inner, path, entries: Mutex::new(Vec::new()) }
    }
}

impl<P: ModelProvider> ModelProvider for Recording<P> {
    fn complete<'a>(&'a self, body: &'a [u8]) -> BoxFuture<'a, ProviderResult> {
        Box::pin(async move {
            let result = self.inner.complete(body).await;
            if let ProviderResult::Response(bytes, _) = &result
                && let Ok(response) = serde_json::from_slice::<serde_json::Value>(bytes)
            {
                let transcript = {
                    let mut entries = self.entries.lock().expect("recording lock");
                    entries.push(TranscriptEntry { expect_request_digest: Some(Digest::of(body)), response });
                    Transcript { responses: entries.clone() }
                };
                let json = serde_json::to_vec_pretty(&transcript).expect("a transcript serializes");
                if let Err(e) = atomic_write(&self.path, &json) {
                    tracing::warn!("recording {}: {e}", self.path.display());
                }
            }
            result
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(i: u32) -> TranscriptEntry {
        TranscriptEntry {
            expect_request_digest: None,
            response: serde_json::json!({"id": format!("msg_{i}"), "usage": {"input_tokens": 10 + i, "output_tokens": 1}}),
        }
    }

    fn body(messages: usize) -> Vec<u8> {
        let m: Vec<_> = (0..messages).map(|i| serde_json::json!({"role": if i % 2 == 0 { "user" } else { "assistant" }, "content": "x"})).collect();
        serde_json::to_vec(&serde_json::json!({ "messages": m })).unwrap()
    }

    fn id_of(r: &ProviderResult) -> String {
        match r {
            ProviderResult::Response(b, _) => serde_json::from_slice::<serde_json::Value>(b).unwrap()["id"].as_str().unwrap().to_string(),
            other => panic!("not a response: {other:?}"),
        }
    }

    #[tokio::test]
    async fn transcript_answers_by_conversation_depth() {
        let p = FakeProvider::from_transcript(Transcript { responses: vec![entry(0), entry(1), entry(2)] });
        assert_eq!(id_of(&p.complete(&body(1)).await), "msg_0");
        assert_eq!(id_of(&p.complete(&body(3)).await), "msg_1");
        assert_eq!(id_of(&p.complete(&body(5)).await), "msg_2");
        assert_eq!(id_of(&p.complete(&body(1)).await), "msg_0", "a retry gets the same answer");
        match p.complete(&body(7)).await {
            ProviderResult::Rejected { status: 400, body } => assert!(body.contains("exhausted"), "{body}"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(p.complete(&body(2)).await, ProviderResult::Rejected { .. }));
        assert_eq!(p.calls(), 6);
    }

    #[tokio::test]
    async fn transcript_digest_pin_rejects_another_request() {
        let pinned = body(1);
        let mut e = entry(0);
        e.expect_request_digest = Some(Digest::of(&pinned));
        let p = FakeProvider::from_transcript(Transcript { responses: vec![e] });
        assert_eq!(id_of(&p.complete(&pinned).await), "msg_0");
        let other = serde_json::to_vec(&serde_json::json!({"messages": [{"role": "user", "content": "different"}]})).unwrap();
        match p.complete(&other).await {
            ProviderResult::Rejected { status: 400, body } => assert!(body.contains("entry 0 expects request"), "{body}"),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn scripted_answers_in_order_then_rejects() {
        let p = FakeProvider::scripted(vec![
            ProviderResult::Transport("boom".into()),
            ProviderResult::Response(b"{}".to_vec(), Default::default()),
        ]);
        assert_eq!(p.complete(b"a").await, ProviderResult::Transport("boom".into()));
        assert!(matches!(p.complete(b"b").await, ProviderResult::Response(..)));
        assert_eq!(
            p.complete(b"c").await,
            ProviderResult::Rejected { status: 400, body: "scripted provider exhausted".into() }
        );
        assert_eq!(p.calls(), 3);
    }

    #[tokio::test]
    async fn recording_writes_the_transcript_format_without_the_request_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rec.json");
        let rec = Recording::new(FakeProvider::from_transcript(Transcript { responses: vec![entry(0), entry(1)] }), path.clone());
        let b1 = br#"{"messages":[{"role":"user","content":"PRIVATE-REQUEST-TEXT"}]}"#.to_vec();
        let b2 = br#"{"messages":[{"role":"user","content":"PRIVATE-REQUEST-TEXT"},{"role":"assistant","content":"a"},{"role":"user","content":"PRIVATE-2"}]}"#.to_vec();
        rec.complete(&b1).await;
        rec.complete(&b2).await;
        rec.complete(b"not json").await; // rejected: unrecorded
        let t = load_transcript(&path).unwrap();
        assert_eq!(t.responses.len(), 2);
        assert_eq!(t.responses[0].expect_request_digest, Some(Digest::of(&b1)));
        assert_eq!(t.responses[1].expect_request_digest, Some(Digest::of(&b2)));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("PRIVATE"));
        // and it replays
        let replay = FakeProvider::from_file(&path).unwrap();
        assert_eq!(id_of(&replay.complete(&b1).await), "msg_0");
    }
}
