//! A failed host publication never settles the effect; recovery uses its retained receipt.
mod common;

use agentos_core::broker::Resource;
use agentos_core::effect::{EffectId, EffectKind, EffectState};
use agentos_core::ids::Digest;
use agentos_core::state::TaskEvent;
use agentos_engine::executor::{AttemptCtx, EffectRequest, ExecOutcome, Executor};
use agentos_engine::{recover::recover, steps};
use agentos_store::blob::{BlobStore, PublicationStage};
use common::Env;

struct Retained(ExecOutcome);
impl Executor for Retained {
    async fn run(&self, _: &EffectRequest, _: &AttemptCtx) -> ExecOutcome {
        panic!("publication recovery must not reexecute")
    }
    fn retained_outcome(&self, id: &EffectId) -> Option<ExecOutcome> {
        assert_eq!(*id, self.0.receipt.effect_id);
        Some(self.0.clone())
    }
}

#[tokio::test]
async fn enospc_at_each_publication_boundary_keeps_effect_unsettled_until_recovery() {
    for stage in [
        PublicationStage::FileSync,
        PublicationStage::Rename,
        PublicationStage::DirectorySync,
    ] {
        let env = Env::new(10);
        env.db.append(&env.task, &TaskEvent::Started).unwrap();
        let base = env.db.task(&env.task).unwrap().workspace_digest;
        let rec = steps::intend(
            &env.db,
            &env.task,
            EffectKind::ReadSnapshot,
            Digest::of(b""),
            &base,
            &Resource::Task,
        )
        .unwrap();
        let ctx = steps::dispatch(&env.db, &rec).unwrap();
        let out = steps::execute(&env.exec, &rec, Vec::new(), &env.contract, 0, &ctx)
            .await
            .unwrap();
        let faulty = BlobStore::open(env.dir.path().join("blobs"))
            .unwrap()
            .with_publication_hook(move |at| {
                if at == stage {
                    Err(std::io::Error::from_raw_os_error(28))
                } else {
                    Ok(())
                }
            });
        let error = steps::store_result(&faulty, &out).expect_err("ENOSPC must refuse publication");
        assert!(error.to_string().contains("No space left"), "{error}");
        let after = env.db.effect(&rec.effect_id).unwrap();
        assert_eq!(after.state, EffectState::Dispatched);
        assert_eq!(after.result_digest, None);
        assert!(
            !env.db
                .referenced_blobs()
                .unwrap()
                .contains(&Digest::of(&out.output))
        );
        assert_eq!(
            std::fs::read_dir(env.dir.path().join("blobs/tmp"))
                .unwrap()
                .count(),
            0
        );
        let retained = Retained(out.clone());
        recover(&env.db, &env.blobs, &retained, &env.task)
            .await
            .unwrap();
        assert_eq!(
            env.db.effect(&rec.effect_id).unwrap().state,
            EffectState::Completed
        );
        assert_eq!(env.blobs.get(&Digest::of(&out.output)).unwrap(), out.output);
        assert!(
            recover(&env.db, &env.blobs, &retained, &env.task)
                .await
                .unwrap()
                .decisions
                .is_empty()
        );
    }
}
