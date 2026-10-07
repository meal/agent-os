//! A failed host publication never settles the effect; recovery uses its retained receipt.
//! Driven through the real runner path (`run_task` → `finish_attempt`), not `store_result`.
mod common;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agentos_core::effect::{EffectId, EffectKind, EffectState};
use agentos_core::ids::{Digest, TaskId};
use agentos_core::state::TaskState;
use agentos_engine::agent::FakeAgent;
use agentos_engine::executor::{AttemptCtx, EffectRequest, ExecOutcome, Executor};
use agentos_engine::fixture::FixtureExecutor;
use agentos_engine::{recover::recover, runner::run_task};
use agentos_store::blob::{BlobStore, PublicationStage};
use common::{Env, fix_patch};

/// The fixture executor, counting runs, retaining each outcome (as job directories do)
/// and arming the publication fault once an effect has really executed.
struct Retaining {
    inner: FixtureExecutor,
    runs: AtomicUsize,
    armed: Arc<AtomicBool>,
    retained: Mutex<Vec<ExecOutcome>>,
}
impl Executor for Retaining {
    async fn run(&self, req: &EffectRequest, ctx: &AttemptCtx) -> ExecOutcome {
        let out = self.inner.run(req, ctx).await;
        self.runs.fetch_add(1, Ordering::SeqCst);
        self.retained.lock().unwrap().push(out.clone());
        self.armed.store(true, Ordering::SeqCst);
        out
    }
    fn retained_outcome(&self, id: &EffectId) -> Option<ExecOutcome> {
        self.retained
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|o| o.receipt.effect_id == *id)
            .cloned()
    }
    fn current_workspace(&self, task: &TaskId) -> Option<Result<Digest, String>> {
        self.inner.current_workspace(task)
    }
}

#[tokio::test]
async fn enospc_publishing_a_runner_result_keeps_the_effect_unsettled_until_recovery() {
    for stage in [
        PublicationStage::FileSync,
        PublicationStage::Rename,
        PublicationStage::DirectorySync,
    ] {
        let env = Env::new(10);
        let armed = Arc::new(AtomicBool::new(false));
        let fault = armed.clone();
        let faulty = BlobStore::open(env.dir.path().join("blobs"))
            .unwrap()
            .with_publication_hook(move |at| {
                if fault.load(Ordering::SeqCst) && at == stage {
                    Err(std::io::Error::from_raw_os_error(28))
                } else {
                    Ok(())
                }
            });
        let exec = Retaining {
            inner: env.fixture_exec(),
            runs: AtomicUsize::new(0),
            armed,
            retained: Mutex::new(Vec::new()),
        };
        let mut agent = FakeAgent::from_fixture_patch(fix_patch());
        let error = run_task(&env.db, &faulty, &exec, &mut agent, &env.task)
            .await
            .expect_err("ENOSPC must stop the runner before the effect settles");
        assert!(
            error.to_string().contains("No space left"),
            "{stage:?}: {error}"
        );
        assert_eq!(exec.runs.load(Ordering::SeqCst), 1, "{stage:?}");
        let out = exec.retained.lock().unwrap()[0].clone();
        let effect = env.db.effect(&out.receipt.effect_id).unwrap();
        assert_eq!(effect.kind, EffectKind::ReadSnapshot);
        assert_eq!(effect.state, EffectState::Dispatched, "{stage:?}");
        assert_eq!(effect.result_digest, None);
        assert!(
            !env.db
                .referenced_blobs()
                .unwrap()
                .contains(&Digest::of(&out.output)),
            "{stage:?}: no artifact reference without a published blob"
        );
        assert_eq!(
            std::fs::read_dir(env.dir.path().join("blobs/tmp"))
                .unwrap()
                .count(),
            0,
            "{stage:?}"
        );
        assert!(!env.db.task(&env.task).unwrap().state.is_terminal());

        // The fault is cleared (a healthy store); recovery publishes the retained receipt.
        recover(&env.db, &env.blobs, &exec, &env.task)
            .await
            .unwrap();
        let effect = env.db.effect(&out.receipt.effect_id).unwrap();
        assert_eq!(effect.state, EffectState::Completed, "{stage:?}");
        assert_eq!(effect.result_digest, Some(Digest::of(&out.output)));
        assert_eq!(env.blobs.get(&Digest::of(&out.output)).unwrap(), out.output);
        assert_eq!(exec.runs.load(Ordering::SeqCst), 1, "never re-executed");

        // The task then finishes normally on the healthy store.
        let mut agent = FakeAgent::from_fixture_patch(fix_patch());
        let state = run_task(&env.db, &env.blobs, &exec, &mut agent, &env.task)
            .await
            .unwrap();
        assert_eq!(state, TaskState::Succeeded, "{stage:?}");
    }
}
