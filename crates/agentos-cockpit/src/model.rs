use agentos_core::ids::{TaskId, WorkflowId};
use agentos_core::state::TaskState;
use agentos_core::workflow::display_label;
use agentos_store::view::Snapshot;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Workflow(WorkflowId),
    Task(TaskId),
}

/// One line of the list: a workflow, or a task that belongs to no workflow.
#[derive(Debug, Clone)]
pub struct Item {
    pub key: Key,
    pub kind: &'static str,
    pub id: String,
    pub name: String,
    pub state: String,
    pub progress: String,
    pub model_used: u64,
    pub model_limit: u64,
    pub tool_used: u64,
    pub tool_limit: u64,
    pub created_ts: i64,
}

/// Workflows and tasks without a workflow, newest first. A workflow's step tasks appear only
/// through their workflow.
pub fn items(snapshot: &Snapshot) -> Vec<Item> {
    let workflows = snapshot.workflows.iter().map(|w| {
        let done = w
            .steps
            .iter()
            .filter(|s| s.task_state == Some(TaskState::Succeeded))
            .count();
        Item {
            key: Key::Workflow(w.id.clone()),
            kind: "workflow",
            id: w.id.to_string(),
            name: w.name.clone(),
            state: display_label(w.state(), w.approved).to_string(),
            progress: format!("{done}/{}", w.steps.len()),
            model_used: w.steps.iter().map(|s| s.model_used).sum(),
            model_limit: w.steps.iter().map(|s| u64::from(s.model_limit)).sum(),
            tool_used: w.steps.iter().map(|s| s.tool_used).sum(),
            tool_limit: w.steps.iter().map(|s| u64::from(s.tool_limit)).sum(),
            created_ts: w.created_ts,
        }
    });
    let tasks = snapshot
        .tasks
        .iter()
        .filter(|t| t.workflow.is_none())
        .map(|t| {
            let mut state = t.state.label().to_string();
            if t.cancel_requested && !t.state.is_terminal() {
                state.push_str(" (cancelling)");
            }
            Item {
                key: Key::Task(t.id.clone()),
                kind: "task",
                id: t.id.to_string(),
                name: t.goal.clone(),
                state,
                progress: String::new(),
                model_used: t.model_used,
                model_limit: u64::from(t.model_limit),
                tool_used: t.tool_used,
                tool_limit: u64::from(t.tool_limit),
                created_ts: t.created_ts,
            }
        });
    let mut all: Vec<Item> = workflows.chain(tasks).collect();
    all.sort_by(|a, b| {
        b.created_ts
            .cmp(&a.created_ts)
            .then_with(|| a.id.cmp(&b.id))
    });
    all
}

/// How long ago `ts` was, as `5s`, `3m`, `2h` or `4d`.
pub fn age(now: i64, ts: i64) -> String {
    let d = now.saturating_sub(ts).max(0);
    match d {
        0..=59 => format!("{d}s"),
        60..=3599 => format!("{}m", d / 60),
        3600..=86399 => format!("{}h", d / 3600),
        _ => format!("{}d", d / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentos_store::view::{StepView, TaskRow, WorkflowRow};

    #[test]
    fn age_picks_the_largest_whole_unit() {
        assert_eq!(age(100, 95), "5s");
        assert_eq!(age(1000, 700), "5m");
        assert_eq!(age(10_000, 2_800), "2h");
        assert_eq!(age(400_000, 0), "4d");
        assert_eq!(age(5, 10), "0s");
    }

    fn task(state: TaskState, created_ts: i64, workflow: Option<(WorkflowId, usize)>) -> TaskRow {
        TaskRow {
            id: TaskId::new(),
            state,
            cancel_requested: false,
            goal: "goal".into(),
            model_limit: 10,
            tool_limit: 20,
            model_used: 1,
            tool_used: 2,
            created_ts,
            workflow,
        }
    }

    fn step(idx: usize, task: Option<&TaskRow>) -> StepView {
        StepView {
            idx,
            name: format!("step-{idx}"),
            task_id: task.map(|t| t.id.clone()),
            skipped: false,
            task_state: task.map(|t| t.state),
            model_limit: 3,
            tool_limit: 4,
            model_used: if task.is_some() { 1 } else { 0 },
            tool_used: if task.is_some() { 2 } else { 0 },
        }
    }

    fn workflow(
        created_ts: i64,
        approved: bool,
        failed: bool,
        steps: Vec<StepView>,
    ) -> WorkflowRow {
        WorkflowRow {
            id: WorkflowId::new(),
            name: "chain".into(),
            approved,
            cancel_requested: false,
            failed,
            created_ts,
            steps,
        }
    }

    fn snapshot(tasks: Vec<TaskRow>, workflows: Vec<WorkflowRow>) -> Snapshot {
        Snapshot {
            tasks,
            workflows,
            more: false,
        }
    }

    #[test]
    fn items_merge_workflows_and_loose_tasks_newest_first() {
        let wf_id = WorkflowId::new();
        let step_task = task(TaskState::Succeeded, 150, Some((wf_id.clone(), 0)));
        let old = task(TaskState::Running, 100, None);
        let new = task(TaskState::Failed, 300, None);
        let mut wf = workflow(
            200,
            false,
            false,
            vec![step(0, Some(&step_task)), step(1, None)],
        );
        wf.id = wf_id;
        let snap = snapshot(
            vec![new.clone(), step_task.clone(), old.clone()],
            vec![wf.clone()],
        );

        let items = items(&snap);

        assert_eq!(items.len(), 3, "the step task is not its own item");
        assert!(items.iter().all(|i| i.id != step_task.id.to_string()));
        assert_eq!(
            items.iter().map(|i| i.created_ts).collect::<Vec<_>>(),
            vec![300, 200, 100]
        );
        assert_eq!(items[0].key, Key::Task(new.id.clone()));
        assert_eq!(items[0].kind, "task");
        assert_eq!(items[0].state, "FAILED");

        let w = &items[1];
        assert_eq!(w.key, Key::Workflow(wf.id.clone()));
        assert_eq!(w.kind, "workflow");
        assert_eq!(w.name, "chain");
        assert_eq!(w.progress, "1/2");
        assert_eq!((w.model_used, w.model_limit), (1, 6));
        assert_eq!((w.tool_used, w.tool_limit), (2, 8));
        assert_eq!(w.state, "READY", "running but not approved");
    }

    #[test]
    fn a_failed_workflow_shows_the_failed_label() {
        let wf = workflow(1, true, true, vec![step(0, None)]);
        let items = items(&snapshot(vec![], vec![wf]));
        assert_eq!(items[0].state, "FAILED");
        assert_eq!(items[0].progress, "0/1");
    }

    #[test]
    fn ties_break_by_id_and_a_cancelling_task_says_so() {
        let mut a = task(TaskState::Running, 5, None);
        a.cancel_requested = true;
        let mut done = task(TaskState::Cancelled, 5, None);
        done.cancel_requested = true;
        let items = items(&snapshot(vec![a.clone(), done.clone()], vec![]));
        let ids: Vec<_> = items.iter().map(|i| i.id.clone()).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted);
        let state = |t: &TaskRow| {
            items
                .iter()
                .find(|i| i.id == t.id.to_string())
                .unwrap()
                .state
                .clone()
        };
        assert_eq!(state(&a), "RUNNING (cancelling)");
        assert_eq!(state(&done), "CANCELLED");
    }
}
