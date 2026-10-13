use agentos_core::ids::{TaskId, WorkflowId};
use agentos_core::state::TaskState;
use agentos_core::workflow::{WorkflowState, display_label};
use agentos_store::view::{Snapshot, StepView, WorkflowRow};

use crate::sanitize::clean;

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

/// Workflows and tasks without a workflow, newest first. A snapshot holds no step task: those
/// appear only through their workflow.
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
            state: workflow_label(w),
            progress: format!("{done}/{}", w.steps.len()),
            model_used: total(w, |s| s.model_used),
            model_limit: total(w, |s| u64::from(s.model_limit)),
            tool_used: total(w, |s| s.tool_used),
            tool_limit: total(w, |s| u64::from(s.tool_limit)),
            created_ts: w.created_ts,
        }
    });
    let tasks = snapshot.tasks.iter().map(|t| {
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

/// A workflow's state, saying so while a cancel is on its way, as a task's does.
pub fn workflow_label(w: &WorkflowRow) -> String {
    let state = w.state();
    let mut label = display_label(state, w.approved).to_string();
    if w.cancel_requested && state == WorkflowState::Running {
        label.push_str(" (cancelling)");
    }
    label
}

/// One budget figure summed over a workflow's steps; it saturates rather than overflows.
fn total(w: &WorkflowRow, figure: impl Fn(&StepView) -> u64) -> u64 {
    w.steps
        .iter()
        .fold(0u64, |sum, s| sum.saturating_add(figure(s)))
}

/// A payload as display lines, each cleaned: laid out one value per line when it is JSON,
/// as stored otherwise (a payload cut short is no longer JSON).
pub fn payload_lines(text: &str) -> Vec<String> {
    let pretty = pretty_json(text);
    pretty
        .as_deref()
        .unwrap_or(text)
        .split('\n')
        .map(|l| clean(l, usize::MAX))
        .collect()
}

/// `text` re-indented, two spaces a level, when it parses as JSON. It works on the text, not a
/// parsed value, so keys keep their stored order and numbers their stored digits.
fn pretty_json(text: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(text).ok()?;
    let mut out = String::with_capacity(text.len() * 2);
    let mut depth = 0usize;
    let (mut in_string, mut escaped) = (false, false);
    let newline = |out: &mut String, depth: usize| {
        out.push('\n');
        out.push_str(&"  ".repeat(depth));
    };
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '{' | '[' => {
                out.push(c);
                while chars.next_if(|c| c.is_ascii_whitespace()).is_some() {}
                match chars.next_if(|c| matches!(c, '}' | ']')) {
                    Some(close) => out.push(close),
                    None => {
                        depth += 1;
                        newline(&mut out, depth);
                    }
                }
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                newline(&mut out, depth);
                out.push(c);
            }
            ',' => {
                out.push(c);
                newline(&mut out, depth);
            }
            ':' => out.push_str(": "),
            c if c.is_ascii_whitespace() => {}
            c => out.push(c),
        }
    }
    Some(out)
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
    use agentos_store::view::TaskRow;

    #[test]
    fn age_picks_the_largest_whole_unit() {
        assert_eq!(age(100, 95), "5s");
        assert_eq!(age(1000, 700), "5m");
        assert_eq!(age(10_000, 2_800), "2h");
        assert_eq!(age(400_000, 0), "4d");
        assert_eq!(age(5, 10), "0s");
    }

    fn task(state: TaskState, created_ts: i64) -> TaskRow {
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
            skipped: 0,
        }
    }

    #[test]
    fn items_merge_workflows_and_loose_tasks_newest_first() {
        let step_task = task(TaskState::Succeeded, 150);
        let old = task(TaskState::Running, 100);
        let new = task(TaskState::Failed, 300);
        let wf = workflow(
            200,
            false,
            false,
            vec![step(0, Some(&step_task)), step(1, None)],
        );
        let snap = snapshot(vec![new.clone(), old.clone()], vec![wf.clone()]);

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
        let mut a = task(TaskState::Running, 5);
        a.cancel_requested = true;
        let mut done = task(TaskState::Cancelled, 5);
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

    #[test]
    fn a_workflow_being_cancelled_says_so_until_it_ends() {
        let t = task(TaskState::Running, 1);
        let mut wf = workflow(5, true, false, vec![step(0, Some(&t))]);
        wf.cancel_requested = true;
        assert_eq!(
            items(&snapshot(vec![], vec![wf.clone()]))[0].state,
            "RUNNING (cancelling)"
        );
        wf.steps[0].task_state = Some(TaskState::Cancelled);
        assert_eq!(items(&snapshot(vec![], vec![wf]))[0].state, "CANCELLED");
    }

    #[test]
    fn workflow_budgets_saturate_instead_of_overflowing() {
        let mut big = step(0, None);
        big.model_used = u64::MAX;
        big.tool_used = u64::MAX - 1;
        let mut more = step(1, None);
        more.model_used = 5;
        more.tool_used = 5;
        let wf = workflow(1, true, false, vec![big, more]);
        let item = &items(&snapshot(vec![], vec![wf]))[0];
        assert_eq!((item.model_used, item.tool_used), (u64::MAX, u64::MAX));
        assert_eq!((item.model_limit, item.tool_limit), (6, 8));
    }

    #[test]
    fn json_payloads_are_laid_out_one_value_per_line_in_stored_order() {
        let lines = payload_lines(
            r#"{"z":1,"a":[true,{}],"s":"x,\"y\":{z}","e":[],"n":12345678901234567890}"#,
        );
        assert_eq!(
            lines,
            vec![
                "{",
                "  \"z\": 1,",
                "  \"a\": [",
                "    true,",
                "    {}",
                "  ],",
                "  \"s\": \"x,\\\"y\\\":{z}\",",
                "  \"e\": [],",
                "  \"n\": 12345678901234567890",
                "}",
            ]
        );
    }

    #[test]
    fn a_payload_that_is_not_json_or_was_cut_is_shown_raw() {
        assert_eq!(payload_lines("plain, text: {x}"), vec!["plain, text: {x}"]);
        assert_eq!(payload_lines(r#"{"a": [1, 2"#), vec![r#"{"a": [1, 2"#]);
        assert_eq!(payload_lines("two\nlines\x1b"), vec!["two", "lines"]);
    }
}
