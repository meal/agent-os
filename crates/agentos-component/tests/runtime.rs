//! The analyzer runtime against the committed components (`fixtures/components/*/component.wasm`, rebuilt by
//! `scripts/build-components.sh`): granted reads only, guest bounds, host budgets, reports.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use agentos_component::{FileEntry, Limits, Outcome, Runtime, Snapshot, SnapshotError, Tree};

fn component(name: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/components")
        .join(format!("{name}-v1/component.wasm"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Files in memory; authorization fails once `allowed` calls have been authorized.
struct Fake {
    files: BTreeMap<String, Vec<u8>>,
    allowed: u32,
    authorized: Arc<AtomicU32>,
}

impl Snapshot for Fake {
    fn authorize(&self) -> Result<(), String> {
        if self.authorized.fetch_add(1, Ordering::SeqCst) < self.allowed {
            Ok(())
        } else {
            Err("revoked".into())
        }
    }
    fn files(&self) -> Result<Vec<FileEntry>, SnapshotError> {
        Ok(self
            .files
            .iter()
            .map(|(p, b)| FileEntry {
                path: p.clone(),
                size: b.len() as u64,
            })
            .collect())
    }
    fn read(&self, path: &str, offset: u64, len: u32) -> Result<Vec<u8>, SnapshotError> {
        if path.contains("..") {
            return Err(SnapshotError::InvalidPath(path.into()));
        }
        let bytes = self.files.get(path).ok_or(SnapshotError::NotFound)?;
        let start = (offset as usize).min(bytes.len());
        let end = start.saturating_add(len as usize).min(bytes.len());
        Ok(bytes[start..end].to_vec())
    }
}

fn tree(task: &str, files: &[(&str, &[u8])], allowed: u32) -> (Tree, Arc<AtomicU32>) {
    let authorized = Arc::new(AtomicU32::new(0));
    let fake = Fake {
        files: files
            .iter()
            .map(|(p, b)| (p.to_string(), b.to_vec()))
            .collect(),
        allowed,
        authorized: authorized.clone(),
    };
    (
        Tree {
            task: task.into(),
            snapshot: Box::new(fake),
        },
        authorized,
    )
}

/// The hostile analyzer told `mode` over a snapshot holding `extra` files too.
fn hostile(mode: &str, extra: &[(&str, &[u8])], limits: Limits) -> Outcome {
    let mut files: Vec<(&str, &[u8])> = vec![("agentos-mode", mode.as_bytes())];
    files.extend_from_slice(extra);
    let (t, _) = tree("task-1", &files, u32::MAX);
    Runtime::new()
        .unwrap()
        .analyze(&component("hostile-analyzer"), "task-1", t, limits)
}

fn failed(outcome: Outcome) -> String {
    match outcome {
        Outcome::Failed(why) => why,
        other => panic!("expected a definite failure, got {other:?}"),
    }
}

#[test]
fn the_reference_analyzer_reports_on_granted_reads() {
    let (t, authorized) = tree(
        "task-1",
        &[
            ("src/parser.py", b"a = 1\nb = 2\n"),
            ("README", b"one line\n"),
            ("blob.bin", &[0xff, 0xfe]),
        ],
        u32::MAX,
    );
    let out = Runtime::new().unwrap().analyze(
        &component("repo-analyzer"),
        "task-1",
        t,
        Limits::default(),
    );
    let Outcome::Report(report) = out else {
        panic!("{out:?}")
    };
    let v: serde_json::Value = serde_json::from_str(&report).unwrap();
    assert_eq!(v["analyzer"], "repo-analyzer-v1");
    assert_eq!(
        (v["files"].as_u64(), v["bytes"].as_u64()),
        (Some(3), Some(23)),
        "{v}"
    );
    assert_eq!(
        (v["text_files"].as_u64(), v["lines"].as_u64()),
        (Some(2), Some(3)),
        "{v}"
    );
    assert_eq!(
        v["extensions"],
        serde_json::json!({ "": 1, "bin": 1, "py": 1 })
    );
    // One files() call and one read per file, each authorized.
    assert_eq!(authorized.load(Ordering::SeqCst), 4);
}

#[test]
fn without_a_usable_capability_every_call_is_denied() {
    let (t, _) = tree("task-1", &[("a", b"x")], 0);
    let why = failed(Runtime::new().unwrap().analyze(
        &component("repo-analyzer"),
        "task-1",
        t,
        Limits::default(),
    ));
    assert_eq!(why, "the analyzer failed: denied: revoked");
}

#[test]
fn a_revocation_between_two_reads_denies_the_second() {
    // The mode read and the first data read are authorized, the second is not.
    let files: [(&str, &[u8]); 2] = [
        ("agentos-mode", b"read-twice data.txt"),
        ("data.txt", b"hello"),
    ];
    let (t, authorized) = tree("task-1", &files, 2);
    let why = failed(Runtime::new().unwrap().analyze(
        &component("hostile-analyzer"),
        "task-1",
        t,
        Limits::default(),
    ));
    assert_eq!(why, "the analyzer failed: denied: revoked");
    assert_eq!(authorized.load(Ordering::SeqCst), 3);
}

#[test]
fn a_tree_of_another_task_is_denied() {
    let (t, authorized) = tree("task-2", &[("agentos-mode", b"ok")], u32::MAX);
    let why = failed(Runtime::new().unwrap().analyze(
        &component("hostile-analyzer"),
        "task-1",
        t,
        Limits::default(),
    ));
    assert!(
        why.contains("denied: the tree belongs to task task-2, not task-1"),
        "{why}"
    );
    assert_eq!(
        authorized.load(Ordering::SeqCst),
        0,
        "the binding is checked before the broker"
    );
}

#[test]
fn an_infinite_loop_runs_out_of_fuel() {
    let limits = Limits {
        fuel: 50_000_000,
        ..Limits::default()
    };
    assert_eq!(
        failed(hostile("loop", &[], limits)),
        "the analyzer ran out of fuel"
    );
}

#[test]
fn memory_growth_stops_at_the_limit() {
    let limits = Limits {
        memory_bytes: 16 << 20,
        ..Limits::default()
    };
    assert_eq!(
        failed(hostile("grow", &[], limits)),
        "the analyzer exceeded its memory limit"
    );
}

#[test]
fn the_wall_clock_backstop_is_an_infrastructure_failure() {
    let limits = Limits {
        fuel: u64::MAX,
        wall_clock: Duration::from_secs(1),
        ..Limits::default()
    };
    assert_eq!(
        hostile("loop", &[], limits),
        Outcome::Infrastructure("the analyzer exceeded the wall-clock backstop".into())
    );
}

#[test]
fn reports_must_be_bounded_json_objects() {
    let d = Limits::default;
    assert!(
        failed(hostile("oversize", &[], d())).starts_with("analyzer report rejected: "),
        "oversize"
    );
    assert!(failed(hostile("oversize", &[], d())).contains("over the 65536 byte limit"));
    assert!(
        failed(hostile("malformed", &[], d())).starts_with("analyzer report rejected: not JSON")
    );
    assert_eq!(
        failed(hostile("array", &[], d())),
        "analyzer report rejected: not a JSON object"
    );
    assert_eq!(
        failed(hostile("err", &[], d())),
        "the analyzer failed: the analyzer gave up"
    );
    // A report is data: one that claims a pass is accepted as a report and nothing more.
    assert_eq!(
        hostile("claim-pass", &[], d()),
        Outcome::Report("{\"passed\":true,\"verified\":true}".into())
    );
}

#[test]
fn host_calls_have_budgets_fuel_cannot_see() {
    let data: [(&str, &[u8]); 1] = [("data.txt", b"hello")];
    assert_eq!(
        failed(hostile("big-read data.txt", &data, Limits::default())),
        "the analyzer failed: read too large"
    );
    let few_calls = Limits {
        read_calls: 100,
        ..Limits::default()
    };
    assert_eq!(
        failed(hostile("many-reads data.txt", &data, few_calls)),
        "the analyzer failed: read budget exhausted"
    );
    let few_bytes = Limits {
        read_bytes: 1000,
        ..Limits::default()
    };
    assert_eq!(
        failed(hostile("many-reads data.txt", &data, few_bytes)),
        "the analyzer failed: read budget exhausted"
    );
    assert_eq!(
        failed(hostile("read ../etc/passwd", &data, Limits::default())),
        "the analyzer failed: invalid path"
    );
    assert_eq!(
        failed(hostile("read missing.txt", &data, Limits::default())),
        "the analyzer failed: not found"
    );
}

#[test]
fn only_an_analyzer_component_is_accepted() {
    let rt = Runtime::new().unwrap();
    rt.check(&component("repo-analyzer")).unwrap();
    let wasi = wat_component(
        r#"(component
             (import "wasi:cli/environment@0.2.0"
               (instance (export "get-arguments" (func (result (list string)))))))"#,
    );
    let why = rt.check(&wasi).err().unwrap();
    assert!(why.contains("imports wasi:cli/environment@0.2.0"), "{why}");
    let (t, _) = tree("task-1", &[], u32::MAX);
    assert!(failed(rt.analyze(&wasi, "task-1", t, Limits::default())).contains("imports wasi:cli"));
    let empty = wat_component("(component)");
    assert_eq!(
        rt.check(&empty).err().unwrap(),
        "the component does not export analyze"
    );
    let core = wat_component("(module)");
    assert!(
        rt.check(&core)
            .err()
            .unwrap()
            .starts_with("not a component")
    );
    assert!(
        rt.check(b"garbage")
            .err()
            .unwrap()
            .starts_with("not a component")
    );
}

/// Component text: the tests enable Wasmtime's `wat` feature, which parses it.
fn wat_component(text: &str) -> Vec<u8> {
    text.as_bytes().to_vec()
}
