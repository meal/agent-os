//! The analyzer runtime: runs one `agentos:analyzer/analyzer@1.0.0` component over a task's
//! snapshot. The component gets the `snapshot` interface and nothing else (no WASI). Every
//! host call checks the tree's task binding and the task's capability, and counts against
//! read budgets that fuel does not see. Fuel and a memory limiter bound the guest; an epoch
//! deadline is a wall-clock backstop only.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use wasmtime::component::types::ComponentItem;
use wasmtime::component::{Component, HasSelf, Linker, Resource, ResourceTable};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder, Trap};

mod bindings {
    wasmtime::component::bindgen!({
        world: "analyzer",
        path: "../../wit",
        with: { "agentos:analyzer/snapshot.tree": super::TreeHandle },
    });
}

use bindings::agentos::analyzer::snapshot::{Entry, Host, HostTree, ReadError};

/// The interface a component may import, and the export it must have.
pub const SNAPSHOT_INTERFACE: &str = "agentos:analyzer/snapshot@1.0.0";
pub const ANALYZE_EXPORT: &str = "analyze";
/// The Wasmtime release this runtime is built on (part of an analysis's request identity:
/// fuel accounting may change between releases).
pub const RUNTIME: &str = "wasmtime 49.0.2";

/// The bounds of one run. Every field is part of the request identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub fuel: u64,
    pub memory_bytes: usize,
    pub table_elements: usize,
    pub report_bytes: usize,
    /// The most one `read` may ask for.
    pub read_max: u32,
    /// Host calls (`files` and `read`) in one run.
    pub read_calls: u32,
    /// Bytes returned by `read` in one run.
    pub read_bytes: u64,
    /// The wall-clock backstop.
    pub wall_clock: Duration,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            fuel: 2_000_000_000,
            memory_bytes: 64 << 20,
            table_elements: 10_000,
            report_bytes: 64 << 10,
            read_max: 1 << 20,
            read_calls: 16_384,
            read_bytes: 512 << 20,
            wall_clock: Duration::from_secs(60),
        }
    }
}

/// One regular file of a snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
}

/// Why a snapshot read failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotError {
    NotFound,
    InvalidPath(String),
    Io(String),
}

/// The host's view of a task's snapshot. The engine implements it with its broker check and
/// the path validation `ReadFile` uses.
pub trait Snapshot: Send {
    /// Whether the task's `snapshot.analyze` capability is usable right now.
    fn authorize(&self) -> Result<(), String>;
    /// Every regular file, sorted by path.
    fn files(&self) -> Result<Vec<FileEntry>, SnapshotError>;
    /// At most `len` bytes of `path` from `offset`; empty at or past the end.
    fn read(&self, path: &str, offset: u64, len: u32) -> Result<Vec<u8>, SnapshotError>;
}

/// A tree handed to the component: a task's snapshot, bound to that task.
pub struct Tree {
    pub task: String,
    pub snapshot: Box<dyn Snapshot>,
}

/// The host value behind a component's `tree` handle.
pub struct TreeHandle(Tree);

/// What a run produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// A JSON object within the report limit.
    Report(String),
    /// A definite failure: the same component over the same snapshot fails the same way.
    Failed(String),
    /// Not deterministic (the wall-clock backstop, or the host itself failing).
    Infrastructure(String),
}

struct State {
    task: String,
    table: ResourceTable,
    limits: StoreLimits,
    bounds: Limits,
    calls: u32,
    bytes: u64,
}

impl State {
    /// The checks every host call makes: the tree is this run's task's, the capability is
    /// usable, and the call budget is not used up.
    fn admit(&mut self, tree: &Resource<TreeHandle>) -> Result<&Tree, ReadError> {
        if self.calls >= self.bounds.read_calls {
            return Err(ReadError::BudgetExhausted);
        }
        self.calls += 1;
        let handle = self
            .table
            .get(tree)
            .map_err(|e| ReadError::Denied(format!("unknown tree: {e}")))?;
        if handle.0.task != self.task {
            return Err(ReadError::Denied(format!(
                "the tree belongs to task {}, not {}",
                handle.0.task, self.task
            )));
        }
        handle.0.snapshot.authorize().map_err(ReadError::Denied)?;
        Ok(&handle.0)
    }
}

fn read_error(e: SnapshotError) -> ReadError {
    match e {
        SnapshotError::NotFound => ReadError::NotFound,
        SnapshotError::InvalidPath(_) => ReadError::InvalidPath,
        SnapshotError::Io(why) => ReadError::Denied(format!("cannot read: {why}")),
    }
}

impl Host for State {}

impl HostTree for State {
    fn files(&mut self, tree: Resource<TreeHandle>) -> Result<Vec<Entry>, ReadError> {
        let tree = self.admit(&tree)?;
        let files = tree.snapshot.files().map_err(read_error)?;
        Ok(files
            .into_iter()
            .map(|f| Entry {
                path: f.path,
                size: f.size,
            })
            .collect())
    }

    fn read(
        &mut self,
        tree: Resource<TreeHandle>,
        path: String,
        offset: u64,
        len: u32,
    ) -> Result<Vec<u8>, ReadError> {
        let (read_max, read_bytes, used) =
            (self.bounds.read_max, self.bounds.read_bytes, self.bytes);
        if len > read_max {
            return Err(ReadError::TooLarge);
        }
        let tree = self.admit(&tree)?;
        if used.saturating_add(u64::from(len)) > read_bytes {
            return Err(ReadError::BudgetExhausted);
        }
        let bytes = tree.snapshot.read(&path, offset, len).map_err(read_error)?;
        self.bytes += bytes.len() as u64;
        Ok(bytes)
    }

    fn drop(&mut self, tree: Resource<TreeHandle>) -> wasmtime::Result<()> {
        self.table.delete(tree)?;
        Ok(())
    }
}

/// The compiler and the epoch ticker, shared by every run.
pub struct Runtime {
    engine: Engine,
    stop: Arc<AtomicBool>,
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Runtime {
    pub fn new() -> Result<Runtime, String> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        config.consume_fuel(true);
        config.epoch_interruption(true);
        let engine = Engine::new(&config).map_err(|e| format!("cannot start wasmtime: {e}"))?;
        let stop = Arc::new(AtomicBool::new(false));
        let (ticker, done) = (engine.clone(), stop.clone());
        // One epoch per second; a store's deadline is its wall clock in seconds.
        std::thread::Builder::new()
            .name("agentos-component-epoch".into())
            .spawn(move || {
                while !done.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_secs(1));
                    ticker.increment_epoch();
                }
            })
            .map_err(|e| format!("cannot start the epoch ticker: {e}"))?;
        Ok(Runtime { engine, stop })
    }

    /// Compiles `bytes` and checks they are an analyzer: a component whose only import is
    /// the snapshot interface and which exports `analyze`.
    pub fn check(&self, bytes: &[u8]) -> Result<Component, String> {
        let component =
            Component::new(&self.engine, bytes).map_err(|e| format!("not a component: {e:#}"))?;
        let ty = component.component_type();
        for (name, item) in ty.imports(&self.engine) {
            // A type or resource import (the world's `use snapshot.{tree}`) grants nothing;
            // functions, instances, modules and components would.
            let grants_nothing =
                matches!(item.ty, ComponentItem::Type(_) | ComponentItem::Resource(_));
            if name != SNAPSHOT_INTERFACE && !grants_nothing {
                return Err(format!(
                    "the component imports {name}; an analyzer may import only {SNAPSHOT_INTERFACE}"
                ));
            }
        }
        if !ty
            .exports(&self.engine)
            .any(|(name, _)| name == ANALYZE_EXPORT)
        {
            return Err(format!("the component does not export {ANALYZE_EXPORT}"));
        }
        Ok(component)
    }

    /// Runs `bytes` over `tree` as task `task`, within `limits`.
    pub fn analyze(&self, bytes: &[u8], task: &str, tree: Tree, limits: Limits) -> Outcome {
        let component = match self.check(bytes) {
            Ok(c) => c,
            Err(why) => return Outcome::Failed(why),
        };
        let mut linker: Linker<State> = Linker::new(&self.engine);
        if let Err(e) =
            bindings::Analyzer::add_to_linker::<State, HasSelf<State>>(&mut linker, |s| s)
        {
            return Outcome::Infrastructure(format!("cannot link the snapshot interface: {e:#}"));
        }
        let state = State {
            task: task.to_string(),
            table: ResourceTable::new(),
            limits: StoreLimitsBuilder::new()
                .memory_size(limits.memory_bytes)
                .table_elements(limits.table_elements)
                // A wit-bindgen component is a few core instances (the module and its
                // shims) with one linear memory.
                .instances(8)
                .memories(1)
                .trap_on_grow_failure(true)
                .build(),
            bounds: limits,
            calls: 0,
            bytes: 0,
        };
        let mut store = Store::new(&self.engine, state);
        store.limiter(|s| &mut s.limits);
        if let Err(e) = store.set_fuel(limits.fuel) {
            return Outcome::Infrastructure(format!("cannot set fuel: {e:#}"));
        }
        store.set_epoch_deadline(limits.wall_clock.as_secs().max(1));
        let instance = match bindings::Analyzer::instantiate(&mut store, &component, &linker) {
            Ok(i) => i,
            Err(e) => return failure(&e, "cannot instantiate the analyzer"),
        };
        let handle = match store.data_mut().table.push(TreeHandle(tree)) {
            Ok(h) => h,
            Err(e) => return Outcome::Infrastructure(format!("cannot create the tree: {e:#}")),
        };
        let result = instance.call_analyze(&mut store, handle);
        match result {
            Ok(Ok(report)) => check_report(report, limits.report_bytes),
            Ok(Err(why)) => Outcome::Failed(format!("the analyzer failed: {}", bounded(&why))),
            Err(e) => failure(&e, "the analyzer trapped"),
        }
    }
}

/// A trap or error as an outcome: the wall-clock backstop is infrastructure, everything else
/// (fuel, memory, a guest trap, a link failure) is deterministic.
fn failure(e: &wasmtime::Error, context: &str) -> Outcome {
    match e.downcast_ref::<Trap>() {
        Some(Trap::Interrupt) => {
            Outcome::Infrastructure("the analyzer exceeded the wall-clock backstop".into())
        }
        Some(Trap::OutOfFuel) => Outcome::Failed("the analyzer ran out of fuel".into()),
        _ => {
            let text = format!("{e:#}");
            if text.contains("forcing trap when growing memory") {
                Outcome::Failed("the analyzer exceeded its memory limit".into())
            } else {
                Outcome::Failed(format!("{context}: {}", bounded(&text)))
            }
        }
    }
}

fn check_report(report: String, limit: usize) -> Outcome {
    if report.len() > limit {
        return Outcome::Failed(format!(
            "analyzer report rejected: {} bytes, over the {limit} byte limit",
            report.len()
        ));
    }
    match serde_json::from_str::<serde_json::Value>(&report) {
        Ok(v) if v.is_object() => Outcome::Report(report),
        Ok(_) => Outcome::Failed("analyzer report rejected: not a JSON object".into()),
        Err(e) => Outcome::Failed(format!("analyzer report rejected: not JSON: {e}")),
    }
}

/// Guest-chosen text, escaped and cut to 512 characters.
fn bounded(text: &str) -> String {
    let escaped: String = text.chars().flat_map(char::escape_default).collect();
    if escaped.chars().count() > 512 {
        format!("{}…", escaped.chars().take(512).collect::<String>())
    } else {
        escaped
    }
}
