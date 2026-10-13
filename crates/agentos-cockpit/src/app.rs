use agentos_core::contract::Contract;
use agentos_core::ids::{TaskId, WorkflowId};
use agentos_store::view::{EventRow, Snapshot};
use ratatui::crossterm::event::KeyCode;

use crate::model::{Item, Key, items};

/// A payload's text and whether it was cut.
pub type PayloadText = (String, bool);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Screen {
    List,
    Workflow(WorkflowId),
    Task(TaskId),
    Payload { task: TaskId, seq: u64 },
}

/// What the runtime should do after a key. Nothing here writes to the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    None,
    Quit,
    Refresh,
    Watch(Option<TaskId>),
    Older(TaskId, u64),
    Payload(TaskId, u64),
}

#[derive(Debug, Clone)]
pub enum EventUpdate {
    Replace {
        task: TaskId,
        rows: Vec<EventRow>,
        total: u64,
    },
    Append {
        task: TaskId,
        rows: Vec<EventRow>,
        total: u64,
    },
    Prepend {
        task: TaskId,
        rows: Vec<EventRow>,
        total: u64,
    },
}

/// One message from the runtime: the outcome of a read plus whatever the open screen asked for.
#[derive(Debug, Clone)]
pub struct Frame {
    pub snapshot: Result<Snapshot, String>,
    pub now: i64,
    pub events: Option<EventUpdate>,
    pub contract: Option<Contract>,
    pub payload: Option<(TaskId, u64, Option<PayloadText>)>,
}

#[derive(Debug, Clone)]
pub struct App {
    pub screen: Screen,
    pub selected: usize,
    pub snapshot: Option<Snapshot>,
    /// `Some(reason)` after a failed read; the last good snapshot stays on screen.
    pub stale: Option<String>,
    pub events: Vec<EventRow>,
    pub event_total: u64,
    pub contract: Option<Contract>,
    /// The open payload's text and whether it was cut.
    pub payload: Option<PayloadText>,
    pub now: i64,
    /// The screens left behind and the selection each had, so back restores it.
    back: Vec<(Screen, usize)>,
}

impl Default for App {
    fn default() -> Self {
        App::new()
    }
}

impl App {
    pub fn new() -> App {
        App {
            screen: Screen::List,
            selected: 0,
            snapshot: None,
            stale: None,
            events: Vec::new(),
            event_total: 0,
            contract: None,
            payload: None,
            now: 0,
            back: Vec::new(),
        }
    }

    pub fn items(&self) -> Vec<Item> {
        self.snapshot.as_ref().map(items).unwrap_or_default()
    }

    /// How many rows the current screen can select.
    fn rows(&self) -> usize {
        match &self.screen {
            Screen::List => self.items().len(),
            Screen::Workflow(id) => self
                .snapshot
                .iter()
                .flat_map(|s| &s.workflows)
                .find(|w| &w.id == id)
                .map_or(0, |w| w.steps.len()),
            Screen::Task(_) => self.events.len(),
            Screen::Payload { .. } => 0,
        }
    }

    fn clamp(&mut self) {
        self.selected = self.selected.min(self.rows().saturating_sub(1));
    }

    fn open(&mut self, screen: Screen) {
        let from = std::mem::replace(&mut self.screen, screen);
        self.back.push((from, self.selected));
        self.selected = 0;
    }

    fn go_back(&mut self) {
        if let Some((screen, selected)) = self.back.pop() {
            self.screen = screen;
            self.selected = selected;
        }
        if !matches!(self.screen, Screen::Task(_) | Screen::Payload { .. }) {
            self.events.clear();
            self.event_total = 0;
            self.contract = None;
        }
        self.payload = None;
        self.clamp();
    }

    pub fn handle_key(&mut self, key: KeyCode) -> Command {
        match key {
            KeyCode::Char('r') => Command::Refresh,
            KeyCode::Char('q') | KeyCode::Esc => match self.screen {
                Screen::List => Command::Quit,
                Screen::Workflow(_) | Screen::Task(_) => {
                    self.go_back();
                    Command::Watch(None)
                }
                Screen::Payload { .. } => {
                    self.go_back();
                    Command::None
                }
            },
            KeyCode::Up | KeyCode::Char('k') => {
                if let Screen::Task(task) = &self.screen
                    && self.selected == 0
                    && let Some(first) = self.events.first()
                    && first.seq > 1
                {
                    return Command::Older(task.clone(), first.seq);
                }
                self.selected = self.selected.saturating_sub(1);
                Command::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = self.selected.saturating_add(1);
                self.clamp();
                Command::None
            }
            KeyCode::Enter => self.enter(),
            KeyCode::Char('p') => match &self.screen {
                Screen::Task(task) => self
                    .events
                    .get(self.selected)
                    .map_or(Command::None, |e| Command::Payload(task.clone(), e.seq)),
                _ => Command::None,
            },
            _ => Command::None,
        }
    }

    fn enter(&mut self) -> Command {
        match self.screen.clone() {
            Screen::List => match self.items().get(self.selected).map(|i| i.key.clone()) {
                Some(Key::Workflow(id)) => {
                    self.open(Screen::Workflow(id));
                    Command::None
                }
                Some(Key::Task(id)) => self.open_task(id),
                None => Command::None,
            },
            Screen::Workflow(id) => {
                let task = self
                    .snapshot
                    .iter()
                    .flat_map(|s| &s.workflows)
                    .find(|w| w.id == id)
                    .and_then(|w| w.steps.get(self.selected))
                    .and_then(|s| s.task_id.clone());
                task.map_or(Command::None, |t| self.open_task(t))
            }
            Screen::Task(_) | Screen::Payload { .. } => Command::None,
        }
    }

    fn open_task(&mut self, id: TaskId) -> Command {
        self.events.clear();
        self.event_total = 0;
        self.contract = None;
        self.open(Screen::Task(id.clone()));
        Command::Watch(Some(id))
    }

    fn open_task_id(&self) -> Option<&TaskId> {
        match &self.screen {
            Screen::Task(id) | Screen::Payload { task: id, .. } => Some(id),
            _ => None,
        }
    }

    pub fn apply_frame(&mut self, frame: Frame) {
        self.now = frame.now;
        match frame.snapshot {
            Ok(snapshot) => {
                self.snapshot = Some(snapshot);
                self.stale = None;
            }
            Err(reason) => self.stale = Some(reason),
        }
        if let Some(update) = frame.events {
            self.apply_events(update);
        }
        if let Some(contract) = frame.contract
            && matches!(self.screen, Screen::Task(_))
        {
            self.contract = Some(contract);
        }
        if let Some((task, seq, text)) = frame.payload
            && self.screen == Screen::Task(task.clone())
        {
            self.payload = text;
            self.open(Screen::Payload { task, seq });
        }
        self.clamp();
    }

    fn apply_events(&mut self, update: EventUpdate) {
        let (task, total) = match &update {
            EventUpdate::Replace { task, total, .. }
            | EventUpdate::Append { task, total, .. }
            | EventUpdate::Prepend { task, total, .. } => (task.clone(), *total),
        };
        if self.open_task_id() != Some(&task) {
            return;
        }
        self.event_total = total;
        match update {
            EventUpdate::Replace { rows, .. } => {
                self.events = rows;
                self.selected = self.events.len().saturating_sub(1);
            }
            EventUpdate::Append { rows, .. } => {
                let at_tail = self.selected + 1 >= self.events.len();
                let last = self.events.last().map_or(0, |e| e.seq);
                self.events
                    .extend(rows.into_iter().filter(|e| e.seq > last));
                if at_tail {
                    self.selected = self.events.len().saturating_sub(1);
                }
            }
            EventUpdate::Prepend { rows, .. } => {
                let first = self.events.first().map_or(u64::MAX, |e| e.seq);
                let mut rows: Vec<EventRow> = rows.into_iter().filter(|e| e.seq < first).collect();
                self.selected += rows.len();
                rows.append(&mut self.events);
                self.events = rows;
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;
    use agentos_core::contract::Contract;
    use agentos_core::state::TaskState;
    use agentos_store::view::{StepView, TaskRow, WorkflowRow};

    pub fn task(goal: &str, created_ts: i64, workflow: Option<(WorkflowId, usize)>) -> TaskRow {
        TaskRow {
            id: TaskId::new(),
            state: TaskState::Running,
            cancel_requested: false,
            goal: goal.into(),
            model_limit: 10,
            tool_limit: 20,
            model_used: 1,
            tool_used: 2,
            created_ts,
            workflow,
        }
    }

    pub fn step(idx: usize, name: &str, task: Option<&TaskRow>) -> StepView {
        StepView {
            idx,
            name: name.into(),
            task_id: task.map(|t| t.id.clone()),
            skipped: false,
            task_state: task.map(|t| t.state),
            model_limit: 3,
            tool_limit: 4,
            model_used: 1,
            tool_used: 2,
        }
    }

    pub fn workflow(name: &str, created_ts: i64, steps: Vec<StepView>) -> WorkflowRow {
        WorkflowRow {
            id: WorkflowId::new(),
            name: name.into(),
            approved: false,
            cancel_requested: false,
            failed: false,
            created_ts,
            steps,
        }
    }

    pub fn events(seqs: std::ops::RangeInclusive<u64>) -> Vec<EventRow> {
        seqs.map(|seq| EventRow {
            seq,
            event_type: format!("type_{seq}"),
            ts: 90,
        })
        .collect()
    }

    pub fn contract(goal: &str) -> Contract {
        serde_json::from_value(serde_json::json!({
            "goal": goal,
            "repository": {"source": "/repo", "revision": "main"},
            "profile": "default",
            "editable_paths": ["src"],
            "verification_profile": "cargo",
            "capabilities": ["snapshot.read", "model.request"],
            "limits": {
                "model_requests": 7, "max_output_tokens_per_request": 100,
                "tool_actions": 9, "deadline_seconds": 600,
                "worker_vcpus": 1, "worker_memory_mib": 512
            }
        }))
        .unwrap()
    }

    pub fn good(snapshot: Snapshot) -> Frame {
        Frame {
            snapshot: Ok(snapshot),
            now: 100,
            events: None,
            contract: None,
            payload: None,
        }
    }

    /// An app on a task screen with `seqs` as its events, the last one selected.
    pub fn app_on_task(seqs: std::ops::RangeInclusive<u64>) -> (App, TaskId) {
        let t = task("a goal", 10, None);
        let id = t.id.clone();
        let mut app = App::new();
        app.apply_frame(good(Snapshot {
            tasks: vec![t],
            workflows: vec![],
            more: false,
        }));
        assert_eq!(
            app.handle_key(KeyCode::Enter),
            Command::Watch(Some(id.clone()))
        );
        let total = seqs.clone().count() as u64;
        app.apply_frame(Frame {
            events: Some(EventUpdate::Replace {
                task: id.clone(),
                rows: events(seqs),
                total,
            }),
            ..good(app.snapshot.clone().unwrap())
        });
        (app, id)
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    fn snap(
        tasks: Vec<agentos_store::view::TaskRow>,
        wfs: Vec<agentos_store::view::WorkflowRow>,
    ) -> Snapshot {
        Snapshot {
            tasks,
            workflows: wfs,
            more: false,
        }
    }

    /// Every match arm names a variant: adding a variant that writes breaks this on purpose.
    fn is_known(c: &Command) -> bool {
        match c {
            Command::None
            | Command::Quit
            | Command::Refresh
            | Command::Watch(_)
            | Command::Older(..)
            | Command::Payload(..) => true,
        }
    }

    fn all_keys() -> Vec<KeyCode> {
        let mut keys: Vec<KeyCode> = ('a'..='z').map(KeyCode::Char).collect();
        keys.extend(('0'..='9').map(KeyCode::Char));
        keys.extend([
            KeyCode::Enter,
            KeyCode::Esc,
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Tab,
            KeyCode::Delete,
            KeyCode::Backspace,
        ]);
        keys
    }

    /// One app per screen, each with data behind it, plus an empty one.
    fn apps_on_every_screen() -> Vec<App> {
        let mut apps = vec![App::new()];
        let t = task("loose", 50, None);
        let st = task("in wf", 40, None);
        let mut wf = workflow(
            "chain",
            60,
            vec![step(0, "s0", Some(&st)), step(1, "s1", None)],
        );
        wf.steps[0].task_id = Some(st.id.clone());
        let s = snap(vec![t, st], vec![wf]);
        let mut list = App::new();
        list.apply_frame(good(s.clone()));
        apps.push(list.clone());
        let mut on_wf = list.clone();
        on_wf.selected = 0; // newest first: the workflow
        on_wf.handle_key(KeyCode::Enter);
        assert!(matches!(on_wf.screen, Screen::Workflow(_)));
        apps.push(on_wf);
        let (mut on_task, id) = app_on_task(1..=3);
        apps.push(on_task.clone());
        on_task.apply_frame(Frame {
            payload: Some((id, 3, Some(("body".into(), false)))),
            ..good(on_task.snapshot.clone().unwrap())
        });
        assert!(matches!(on_task.screen, Screen::Payload { .. }));
        apps.push(on_task);
        apps
    }

    #[test]
    fn no_key_panics_on_any_screen_and_every_result_is_a_known_command() {
        for app in apps_on_every_screen() {
            for key in all_keys() {
                let mut a = app.clone();
                let cmd = a.handle_key(key);
                assert!(is_known(&cmd), "{key:?} on {:?}", app.screen);
            }
        }
    }

    #[test]
    fn q_on_the_list_quits_and_unbound_keys_do_nothing() {
        let mut app = App::new();
        assert_eq!(app.handle_key(KeyCode::Char('q')), Command::Quit);
        assert_eq!(app.handle_key(KeyCode::Esc), Command::Quit);
        assert_eq!(app.handle_key(KeyCode::Char('z')), Command::None);
        assert_eq!(app.handle_key(KeyCode::Tab), Command::None);
        assert_eq!(app.handle_key(KeyCode::Char('r')), Command::Refresh);
    }

    #[test]
    fn enter_opens_a_loose_task_and_watches_it_and_esc_goes_back() {
        let t = task("loose", 50, None);
        let id = t.id.clone();
        let mut app = App::new();
        app.apply_frame(good(snap(vec![t], vec![])));
        assert_eq!(
            app.handle_key(KeyCode::Enter),
            Command::Watch(Some(id.clone()))
        );
        assert_eq!(app.screen, Screen::Task(id));
        assert_eq!(app.handle_key(KeyCode::Esc), Command::Watch(None));
        assert_eq!(app.screen, Screen::List);
        assert!(app.events.is_empty() && app.contract.is_none());
    }

    #[test]
    fn enter_on_a_task_does_nothing() {
        let (mut app, _) = app_on_task(1..=2);
        let before = app.screen.clone();
        assert_eq!(app.handle_key(KeyCode::Enter), Command::None);
        assert_eq!(app.screen, before);
    }

    #[test]
    fn esc_on_a_task_opened_from_a_workflow_returns_to_that_workflow() {
        let st = task("in wf", 40, None);
        let wf = workflow(
            "chain",
            60,
            vec![step(0, "s0", None), step(1, "s1", Some(&st))],
        );
        let wf_id = wf.id.clone();
        let mut app = App::new();
        app.apply_frame(good(snap(vec![st.clone()], vec![wf])));
        assert_eq!(app.handle_key(KeyCode::Enter), Command::None);
        assert_eq!(app.screen, Screen::Workflow(wf_id.clone()));
        // step 0 has no task: Enter does nothing
        assert_eq!(app.handle_key(KeyCode::Enter), Command::None);
        assert_eq!(app.handle_key(KeyCode::Char('j')), Command::None);
        assert_eq!(app.selected, 1);
        assert_eq!(
            app.handle_key(KeyCode::Enter),
            Command::Watch(Some(st.id.clone()))
        );
        assert_eq!(app.screen, Screen::Task(st.id));
        assert_eq!(app.handle_key(KeyCode::Char('q')), Command::Watch(None));
        assert_eq!(app.screen, Screen::Workflow(wf_id));
        assert_eq!(app.selected, 1, "the step selection is restored");
        assert_eq!(app.handle_key(KeyCode::Esc), Command::Watch(None));
        assert_eq!(app.screen, Screen::List);
    }

    #[test]
    fn up_at_the_first_event_with_older_ones_asks_for_them() {
        let (mut app, id) = app_on_task(5..=7);
        assert_eq!(app.selected, 2, "the newest event is selected");
        assert_eq!(app.handle_key(KeyCode::Up), Command::None);
        assert_eq!(app.handle_key(KeyCode::Char('k')), Command::None);
        assert_eq!(app.selected, 0);
        assert_eq!(app.handle_key(KeyCode::Up), Command::Older(id, 5));
    }

    #[test]
    fn up_at_the_first_event_when_it_is_seq_one_is_a_no_op() {
        let (mut app, _) = app_on_task(1..=3);
        app.selected = 0;
        assert_eq!(app.handle_key(KeyCode::Up), Command::None);
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn down_never_goes_past_the_last_row() {
        let (mut app, _) = app_on_task(1..=3);
        for _ in 0..10 {
            app.handle_key(KeyCode::Down);
        }
        assert_eq!(app.selected, 2);
        let mut list = App::new();
        list.apply_frame(good(snap(
            vec![task("a", 1, None), task("b", 2, None)],
            vec![],
        )));
        for _ in 0..10 {
            list.handle_key(KeyCode::Char('j'));
        }
        assert_eq!(list.selected, 1);
        let mut empty = App::new();
        empty.handle_key(KeyCode::Down);
        assert_eq!(empty.selected, 0);
    }

    #[test]
    fn selection_is_clamped_when_the_list_shrinks() {
        let mut app = App::new();
        app.apply_frame(good(snap(
            vec![task("a", 1, None), task("b", 2, None), task("c", 3, None)],
            vec![],
        )));
        app.handle_key(KeyCode::Down);
        app.handle_key(KeyCode::Down);
        assert_eq!(app.selected, 2);
        app.apply_frame(good(snap(vec![task("a", 1, None)], vec![])));
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn p_asks_for_the_selected_events_payload() {
        let (mut app, id) = app_on_task(1..=3);
        assert_eq!(
            app.handle_key(KeyCode::Char('p')),
            Command::Payload(id.clone(), 3)
        );
        assert!(
            matches!(app.screen, Screen::Task(_)),
            "nothing changes until the frame arrives"
        );
        app.handle_key(KeyCode::Up);
        assert_eq!(app.handle_key(KeyCode::Char('p')), Command::Payload(id, 2));
    }

    #[test]
    fn p_without_events_does_nothing() {
        let t = task("g", 1, None);
        let mut app = App::new();
        app.apply_frame(good(snap(vec![t], vec![])));
        app.handle_key(KeyCode::Enter);
        assert_eq!(app.handle_key(KeyCode::Char('p')), Command::None);
        let mut list = App::new();
        assert_eq!(list.handle_key(KeyCode::Char('p')), Command::None);
    }

    #[test]
    fn a_payload_frame_opens_the_payload_screen_and_esc_returns_to_the_task() {
        let (mut app, id) = app_on_task(1..=3);
        app.handle_key(KeyCode::Up);
        app.apply_frame(Frame {
            payload: Some((id.clone(), 2, Some(("{}".into(), true)))),
            ..good(app.snapshot.clone().unwrap())
        });
        assert_eq!(
            app.screen,
            Screen::Payload {
                task: id.clone(),
                seq: 2
            }
        );
        assert_eq!(app.payload, Some(("{}".into(), true)));
        assert_eq!(app.handle_key(KeyCode::Esc), Command::None);
        assert_eq!(app.screen, Screen::Task(id));
        assert_eq!(app.selected, 1, "the event selection is restored");
        assert!(app.payload.is_none());
    }

    #[test]
    fn a_payload_frame_for_another_task_is_ignored() {
        let (mut app, _) = app_on_task(1..=3);
        app.apply_frame(Frame {
            payload: Some((TaskId::new(), 2, Some(("x".into(), false)))),
            ..good(app.snapshot.clone().unwrap())
        });
        assert!(matches!(app.screen, Screen::Task(_)));
    }

    #[test]
    fn a_failed_frame_keeps_the_snapshot_and_marks_stale_and_a_good_one_clears_it() {
        let mut app = App::new();
        app.apply_frame(good(snap(vec![task("a", 1, None)], vec![])));
        app.apply_frame(Frame {
            snapshot: Err("database is locked".into()),
            now: 200,
            events: None,
            contract: None,
            payload: None,
        });
        assert_eq!(app.stale.as_deref(), Some("database is locked"));
        assert_eq!(app.snapshot.as_ref().unwrap().tasks.len(), 1);
        assert_eq!(app.now, 200);
        app.apply_frame(good(snap(
            vec![task("a", 1, None), task("b", 2, None)],
            vec![],
        )));
        assert!(app.stale.is_none());
        assert_eq!(app.items().len(), 2);
    }

    #[test]
    fn prepend_keeps_the_selection_on_the_same_event() {
        let (mut app, id) = app_on_task(5..=7);
        app.handle_key(KeyCode::Up); // seq 6
        assert_eq!(app.events[app.selected].seq, 6);
        app.apply_frame(Frame {
            events: Some(EventUpdate::Prepend {
                task: id,
                rows: events(2..=4),
                total: 6,
            }),
            ..good(app.snapshot.clone().unwrap())
        });
        assert_eq!(app.events.len(), 6);
        assert_eq!(app.selected, 4);
        assert_eq!(app.events[app.selected].seq, 6);
        assert_eq!(app.event_total, 6);
    }

    #[test]
    fn append_follows_the_tail_only_when_the_tail_was_selected() {
        let (mut app, id) = app_on_task(1..=3);
        let append = |app: &mut App, rows, total| {
            app.apply_frame(Frame {
                events: Some(EventUpdate::Append {
                    task: id.clone(),
                    rows,
                    total,
                }),
                ..good(app.snapshot.clone().unwrap())
            });
        };
        append(&mut app, events(4..=5), 5);
        assert_eq!(app.selected, 4, "was on the newest event, follows it");
        app.handle_key(KeyCode::Up);
        append(&mut app, events(6..=6), 6);
        assert_eq!(app.selected, 3, "reading an older event, stays put");
        assert_eq!(app.events.len(), 6);
    }

    #[test]
    fn event_updates_for_another_task_are_ignored() {
        let (mut app, _) = app_on_task(1..=3);
        app.apply_frame(Frame {
            events: Some(EventUpdate::Replace {
                task: TaskId::new(),
                rows: events(1..=9),
                total: 9,
            }),
            ..good(app.snapshot.clone().unwrap())
        });
        assert_eq!(app.events.len(), 3);
    }

    #[test]
    fn a_contract_frame_is_kept_on_the_task_screen() {
        let (mut app, _) = app_on_task(1..=1);
        app.apply_frame(Frame {
            contract: Some(contract("the goal")),
            ..good(app.snapshot.clone().unwrap())
        });
        assert_eq!(app.contract.unwrap().goal, "the goal");
    }
}
