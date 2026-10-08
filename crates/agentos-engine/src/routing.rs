//! Sends each effect kind to the executor that owns it: model calls to the model executor,
//! file reads to the shadow reader, the analysis to the analysis executor (when the task has
//! an analyzer), everything else to the job executor.

use agentos_core::effect::{EffectId, EffectKind};
use agentos_core::ids::{Digest, TaskId};

use crate::analysis::AnalysisExecutor;
use crate::executor::{AttemptCtx, EffectRequest, ExecOutcome, Executor, JobWait, Reconciliation};
use crate::model::executor::ModelExecutor;
use crate::shadow::ShadowReader;

pub struct RoutingExecutor<J: Executor> {
    pub jobs: J,
    pub model: ModelExecutor,
    pub reads: ShadowReader,
    pub analysis: Option<AnalysisExecutor>,
}

impl<J: Executor> RoutingExecutor<J> {
    pub fn new(jobs: J, model: ModelExecutor, reads: ShadowReader) -> Self {
        RoutingExecutor {
            jobs,
            model,
            reads,
            analysis: None,
        }
    }

    /// Runs the task's `AnalyzeSnapshot` effects with `analysis`.
    pub fn with_analysis(mut self, analysis: AnalysisExecutor) -> Self {
        self.analysis = Some(analysis);
        self
    }
}

impl<J: Executor + Sync> Executor for RoutingExecutor<J> {
    async fn run(&self, req: &EffectRequest, ctx: &AttemptCtx) -> ExecOutcome {
        match req.kind {
            EffectKind::ModelCall { .. } => self.model.run(req, ctx).await,
            EffectKind::ListFiles { .. } | EffectKind::ReadFile { .. } => {
                self.reads.run(req, ctx).await
            }
            EffectKind::AnalyzeSnapshot => match &self.analysis {
                Some(analysis) => analysis.run(req, ctx).await,
                None => ExecOutcome::failure(req, ctx, "no analyzer is configured for this task"),
            },
            _ => self.jobs.run(req, ctx).await,
        }
    }

    fn retained_outcome(&self, effect: &EffectId) -> Option<ExecOutcome> {
        self.jobs
            .retained_outcome(effect)
            .or_else(|| self.model.retained_outcome(effect))
            .or_else(|| {
                self.analysis
                    .as_ref()
                    .and_then(|a| a.retained_outcome(effect))
            })
    }

    async fn reconcile(&self, req: &EffectRequest, ctx: &AttemptCtx) -> Reconciliation {
        self.jobs.reconcile(req, ctx).await
    }

    fn current_workspace(&self, task: &TaskId) -> Option<Result<Digest, String>> {
        self.jobs.current_workspace(task)
    }

    async fn await_job(&self, effect: &EffectId) -> JobWait {
        self.jobs.await_job(effect).await
    }

    async fn fence_job(&self, effect: &EffectId) -> bool {
        self.jobs.fence_job(effect).await
    }
}
