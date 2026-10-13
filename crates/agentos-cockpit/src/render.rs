use agentos_store::view::{StepView, WorkflowRow};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap};

use crate::app::{App, Screen};
use crate::model::{Item, age, workflow_label};
use crate::sanitize::clean;

const EMPTY_HINT: &str =
    "no tasks or workflows yet — try 'agentos submit' or 'agentos workflow submit'";
/// Cap on any one cell's text; the terminal clips what does not fit.
const CELL: usize = 200;

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);
    frame.render_widget(Paragraph::new(header_line(app, area.width)), header);
    match &app.screen {
        Screen::List => draw_list(frame, app, body),
        Screen::Workflow(id) => draw_workflow(frame, app, id, body),
        Screen::Task(_) => draw_task(frame, app, body),
        Screen::Payload { seq, .. } => draw_payload(frame, app, *seq, body),
    }
    frame.render_widget(Paragraph::new(footer_text(&app.screen)), footer);
}

fn header_line(app: &App, width: u16) -> String {
    let width = usize::from(width);
    let mut line = format!("agentos cockpit · {} items", app.items().len());
    // STALE comes before the note, so a narrow terminal cuts the note first.
    if let Some(reason) = &app.stale {
        line.push_str(&format!(" · STALE: {}", clean(reason, width)));
    }
    if let Some(snapshot) = &app.snapshot {
        if snapshot.skipped > 0 {
            line.push_str(&format!(" · {} unreadable rows skipped", snapshot.skipped));
        }
        if snapshot.more {
            line.push_str(&format!(" · showing newest {}", app.items().len()));
        }
    }
    clean(&line, width)
}

fn footer_text(screen: &Screen) -> &'static str {
    match screen {
        Screen::List => "↑↓ move · Enter open · r refresh · q quit",
        Screen::Workflow(_) => "↑↓ move · Enter open · Esc back · r refresh · q back",
        Screen::Task(_) => "↑↓ move · Esc back · p payload · r refresh · q back",
        Screen::Payload { .. } => "↑↓ scroll · PgUp/PgDn page · Esc back · r refresh · q back",
    }
}

fn reversed() -> Style {
    Style::default().add_modifier(Modifier::REVERSED)
}

fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

fn usage(used: u64, limit: u64) -> String {
    format!("{used}/{limit}")
}

fn item_cells(item: &Item, now: i64) -> [String; 8] {
    [
        item.kind.to_string(),
        clean(&item.id.chars().take(8).collect::<String>(), 8),
        clean(&item.name, CELL),
        clean(&item.state, 24),
        item.progress.clone(),
        usage(item.model_used, item.model_limit),
        usage(item.tool_used, item.tool_limit),
        age(now, item.created_ts),
    ]
}

const LIST_HEADER: [&str; 8] = [
    "KIND", "ID", "NAME", "STATE", "STEPS", "MODEL", "TOOLS", "AGE",
];

fn draw_list(frame: &mut Frame, app: &App, area: Rect) {
    if app.snapshot.is_none() {
        // Nothing read yet: say why rather than call the home empty.
        let text = match &app.stale {
            Some(reason) => format!("could not read the database: {}", clean(reason, CELL)),
            None => "loading…".to_string(),
        };
        frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), area);
        return;
    }
    let items = app.items();
    if items.is_empty() {
        frame.render_widget(Paragraph::new(EMPTY_HINT).wrap(Wrap { trim: false }), area);
        return;
    }
    let cells: Vec<[String; 8]> = items.iter().map(|i| item_cells(i, app.now)).collect();
    // Every column but NAME is as wide as its widest cell, so NAME gets the remainder.
    let widths: Vec<Constraint> = (0..LIST_HEADER.len())
        .map(|c| {
            if c == 2 {
                return Constraint::Fill(1);
            }
            let widest = cells
                .iter()
                .map(|row| row[c].chars().count())
                .chain([LIST_HEADER[c].len()])
                .max()
                .unwrap_or(0);
            Constraint::Length(u16::try_from(widest).unwrap_or(u16::MAX))
        })
        .collect();
    let rows: Vec<Row> = cells.into_iter().map(Row::new).collect();
    let table = Table::new(rows, widths)
        .header(Row::new(LIST_HEADER).style(bold()))
        .row_highlight_style(reversed());
    let mut state = TableState::default().with_selected(Some(app.selected));
    frame.render_stateful_widget(table, area, &mut state);
}

fn step_row(step: &StepView) -> Row<'static> {
    let state = match (step.task_state, step.skipped) {
        (Some(state), _) => state.label(),
        (None, true) => "skipped",
        (None, false) => "pending",
    };
    Row::new(vec![
        format!("#{}", step.idx),
        clean(&step.name, CELL),
        state.to_string(),
        usage(step.model_used, u64::from(step.model_limit)),
        usage(step.tool_used, u64::from(step.tool_limit)),
    ])
}

fn draw_workflow(frame: &mut Frame, app: &App, id: &agentos_core::ids::WorkflowId, area: Rect) {
    let workflow: Option<&WorkflowRow> = app
        .snapshot
        .iter()
        .flat_map(|s| &s.workflows)
        .find(|w| &w.id == id);
    let Some(workflow) = workflow else {
        frame.render_widget(Paragraph::new("workflow not found"), area);
        return;
    };
    let title = format!(
        "{} · {}",
        clean(&workflow.name, CELL),
        workflow_label(workflow)
    );
    let rows: Vec<Row> = workflow.steps.iter().map(step_row).collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(5),
            Constraint::Fill(1),
            Constraint::Length(10),
            Constraint::Length(9),
            Constraint::Length(9),
        ],
    )
    .header(Row::new(["IDX", "STEP", "STATE", "MODEL", "TOOLS"]).style(bold()))
    .block(Block::bordered().title(title))
    .row_highlight_style(reversed());
    let mut state = TableState::default().with_selected(Some(app.selected));
    frame.render_stateful_widget(table, area, &mut state);
}

fn draw_task(frame: &mut Frame, app: &App, area: Rect) {
    let width = usize::from(area.width);
    let goal = match (&app.contract, &app.screen, &app.snapshot) {
        (Some(c), _, _) => Some(c.goal.as_str()),
        (None, Screen::Task(id), Some(snapshot)) => snapshot
            .tasks
            .iter()
            .find(|t| &t.id == id)
            .map(|t| t.goal.as_str()),
        _ => None,
    };
    let mut lines = vec![Line::from(format!(
        "goal: {}",
        clean(goal.unwrap_or("?"), width)
    ))];
    match &app.contract {
        Some(c) => {
            let field =
                |name: &str, value: &str| Line::from(clean(&format!("{name}: {value}"), width));
            let repo = &c.repository;
            lines.push(field(
                "repository",
                &format!("{} @ {}", repo.source, repo.revision),
            ));
            lines.push(field("profile", &c.profile));
            lines.push(field("editable", &c.editable_paths.join(", ")));
            lines.push(field("verification", &c.verification_profile));
            let caps: Vec<String> = c.capabilities.iter().map(|k| format!("{k:?}")).collect();
            lines.push(field("capabilities", &caps.join(", ")));
            let l = &c.limits;
            let limits = [
                format!("model_requests {}", l.model_requests),
                format!(
                    "max_output_tokens_per_request {}",
                    l.max_output_tokens_per_request
                ),
                format!("tool_actions {}", l.tool_actions),
                format!("deadline_seconds {}", l.deadline_seconds),
            ];
            lines.push(field("limits", &limits.join(" · ")));
            let mut worker = vec![
                format!("worker_vcpus {}", l.worker_vcpus),
                format!("worker_memory_mib {}", l.worker_memory_mib),
            ];
            for (name, value) in [
                ("worker_disk_mib", l.worker_disk_mib),
                ("worker_scratch_mib", l.worker_scratch_mib),
                ("worker_disk_bandwidth_mib_s", l.worker_disk_bandwidth_mib_s),
                ("worker_disk_iops", l.worker_disk_iops),
            ] {
                if let Some(v) = value {
                    worker.push(format!("{name} {v}"));
                }
            }
            lines.push(field("worker", &worker.join(" · ")));
        }
        None => lines.push(Line::from("loading contract…")),
    }
    let top = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let [info, events] =
        Layout::vertical([Constraint::Length(top), Constraint::Min(0)]).areas(area);
    frame.render_widget(Paragraph::new(lines), info);

    let items: Vec<ListItem> = app
        .events
        .iter()
        .map(|e| {
            ListItem::new(format!(
                "{:>6}  {}  {}",
                e.seq,
                clean(&e.event_type, CELL),
                age(app.now, e.ts)
            ))
        })
        .collect();
    let title = format!(
        "events (showing {} of {})",
        app.events.len(),
        app.event_total
    );
    let list = List::new(items)
        .block(Block::bordered().title(title))
        .highlight_style(reversed());
    let mut state = ListState::default().with_selected(Some(app.selected));
    frame.render_stateful_widget(list, events, &mut state);
}

fn draw_payload(frame: &mut Frame, app: &App, seq: u64, area: Rect) {
    let truncated = app.payload.as_ref().is_some_and(|(_, cut)| *cut);
    let title = format!(
        "event {seq} payload{}",
        if truncated { " (truncated)" } else { "" }
    );
    let text = match &app.payload {
        // Already split into lines (pretty-printed when JSON) and cleaned when it arrived.
        Some(_) => Text::from(
            app.payload_lines
                .iter()
                .map(|l| Line::from(l.as_str()))
                .collect::<Vec<_>>(),
        ),
        None => Text::from("no payload"),
    };
    let paragraph = Paragraph::new(text)
        .block(Block::bordered().title(title))
        .wrap(Wrap { trim: false });
    // The rows the wrapped text needs, borders included, against the rows there are.
    let rows = paragraph.line_count(area.width.saturating_sub(2));
    let max = u16::try_from(rows.saturating_sub(usize::from(area.height))).unwrap_or(u16::MAX);
    app.payload_view.set((max, area.height.saturating_sub(2)));
    frame.render_widget(paragraph.scroll((app.payload_scroll.min(max), 0)), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::fixtures::*;
    use crate::app::{EventUpdate, Frame, Screen};
    use agentos_store::view::Snapshot;
    use ratatui::crossterm::event::KeyCode;

    fn buffer(app: &App, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn screen(app: &App, w: u16, h: u16) -> String {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buf = terminal.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn app_with(
        tasks: Vec<agentos_store::view::TaskRow>,
        wfs: Vec<agentos_store::view::WorkflowRow>,
    ) -> App {
        let mut app = App::new();
        app.apply_frame(good(Snapshot {
            tasks,
            workflows: wfs,
            more: false,
            skipped: 0,
        }));
        app
    }

    #[test]
    fn an_empty_home_shows_the_hint() {
        let out = screen(&app_with(vec![], vec![]), 100, 10);
        assert!(out.contains("agentos cockpit · 0 items"), "{out}");
        assert!(
            out.contains(
                "no tasks or workflows yet — try 'agentos submit' or 'agentos workflow submit'"
            ),
            "{out}"
        );
    }

    #[test]
    fn the_list_shows_a_workflow_and_a_task_but_not_the_workflows_step_task() {
        let mut step_task = task("hidden step goal", 150);
        step_task.state = agentos_core::state::TaskState::Succeeded;
        let wf = workflow(
            "my-chain",
            200,
            vec![step(0, "a", Some(&step_task)), step(1, "b", None)],
        );
        let loose = task("loose goal", 100);
        let out = screen(&app_with(vec![loose], vec![wf]), 120, 12);
        for want in [
            "KIND",
            "STATE",
            "MODEL",
            "TOOLS",
            "AGE",
            "my-chain",
            "loose goal",
            "1/2",
            "READY",
            "RUNNING",
            "1/10",
        ] {
            assert!(out.contains(want), "missing {want}\n{out}");
        }
        assert!(!out.contains("hidden step goal"), "{out}");
        assert!(out.contains("2 items"), "{out}");
    }

    #[test]
    fn the_header_says_when_the_snapshot_is_cut_or_stale() {
        let mut app = app_with(vec![task("a", 1)], vec![]);
        app.snapshot.as_mut().unwrap().more = true;
        let out = screen(&app, 120, 5);
        assert!(out.contains("showing newest 1"), "{out}");
        app.stale = Some("database is locked".into());
        let out = screen(&app, 120, 5);
        assert!(out.contains("STALE: database is locked"), "{out}");
    }

    #[test]
    fn the_workflow_detail_shows_each_steps_name_and_pending() {
        let mut t = task("g", 1);
        t.state = agentos_core::state::TaskState::Succeeded;
        let mut skipped = step(2, "gamma", None);
        skipped.skipped = true;
        let wf = workflow(
            "chain",
            5,
            vec![step(0, "alpha", Some(&t)), step(1, "beta", None), skipped],
        );
        let mut app = app_with(vec![], vec![wf]);
        app.handle_key(KeyCode::Enter);
        assert!(matches!(app.screen, Screen::Workflow(_)));
        let out = screen(&app, 100, 12);
        for want in [
            "chain",
            "#0",
            "alpha",
            "SUCCEEDED",
            "beta",
            "pending",
            "gamma",
            "skipped",
            "1/3",
            "2/4",
        ] {
            assert!(out.contains(want), "missing {want}\n{out}");
        }
    }

    fn task_app() -> App {
        let (mut app, id) = app_on_task(1..=3);
        app.apply_frame(Frame {
            contract: Some((id.clone(), contract("fix the parser"))),
            events: Some(EventUpdate::Replace {
                task: id,
                rows: events(1..=3),
                total: 12,
            }),
            ..good(app.snapshot.clone().unwrap())
        });
        app
    }

    #[test]
    fn the_task_detail_shows_goal_limits_capabilities_and_event_types() {
        let out = screen(&task_app(), 100, 20);
        for want in [
            "fix the parser",
            "default",
            "SnapshotRead",
            "ModelRequest",
            "model_requests",
            "tool_actions",
            "deadline_seconds",
            "events (showing 3 of 12)",
            "type_1",
            "type_3",
            "10s",
        ] {
            assert!(out.contains(want), "missing {want}\n{out}");
        }
    }

    #[test]
    fn a_task_without_a_contract_yet_still_draws() {
        let (app, _) = app_on_task(1..=2);
        let out = screen(&app, 100, 15);
        assert!(out.contains("type_2"), "{out}");
    }

    #[test]
    fn hostile_text_is_neutralised_in_the_list_and_the_detail() {
        let hostile = "fix \x1b[31m\nparser \u{202e}ok";
        let t = task(hostile, 1);
        let mut app = app_with(vec![t], vec![]);
        let list = buffer(&app, 100, 8);
        app.handle_key(KeyCode::Enter);
        let id = match &app.screen {
            Screen::Task(id) => id.clone(),
            s => panic!("{s:?}"),
        };
        app.apply_frame(Frame {
            contract: Some((id.clone(), contract(hostile))),
            events: Some(EventUpdate::Replace {
                task: id,
                rows: events(1..=1),
                total: 1,
            }),
            ..good(app.snapshot.clone().unwrap())
        });
        let detail = buffer(&app, 100, 15);
        for buf in [&list, &detail] {
            let all: String = buf.content().iter().map(|c| c.symbol()).collect();
            assert!(!all.contains('\x1b') && !all.contains('\u{202e}') && !all.contains('\n'));
        }
        let row = (0..100).map(|x| list[(x, 2)].symbol()).collect::<String>();
        assert!(row.contains("fix [31m parser ok"), "{row}");
    }

    #[test]
    fn hostile_workflow_and_stale_text_is_neutralised() {
        let mut wf = workflow("na\x1bme\u{202e}", 1, vec![step(0, "st\nep", None)]);
        wf.steps[0].name = "st\nep\x1b".into();
        let mut app = app_with(vec![], vec![wf]);
        app.stale = Some("bad\x1b\n\u{202e}".into());
        let buf = buffer(&app, 100, 8);
        let all: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(!all.contains('\x1b') && !all.contains('\u{202e}') && !all.contains('\n'));
        app.handle_key(KeyCode::Enter);
        let buf = buffer(&app, 100, 8);
        let all: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(!all.contains('\x1b') && !all.contains('\u{202e}') && !all.contains('\n'));
    }

    fn payload_app(text: &str, truncated: bool) -> App {
        let mut app = task_app();
        let id = match &app.screen {
            Screen::Task(id) => id.clone(),
            s => panic!("{s:?}"),
        };
        app.handle_key(KeyCode::Char('p'));
        app.apply_frame(Frame {
            payload: Some((id, 3, Some((text.into(), truncated)))),
            ..good(app.snapshot.clone().unwrap())
        });
        app
    }

    #[test]
    fn a_truncated_payload_says_so() {
        let out = screen(&payload_app("{\"a\": [1, 2", true), 100, 10);
        assert!(out.contains("event 3 payload (truncated)"), "{out}");
        assert!(out.contains("{\"a\": [1, 2"), "cut short, shown raw\n{out}");
        let out = screen(&payload_app("{}", false), 100, 10);
        assert!(out.contains("event 3 payload"), "{out}");
        assert!(!out.contains("truncated"), "{out}");
    }

    #[test]
    fn payload_lines_stay_lines_and_control_characters_go() {
        let out = screen(
            &payload_app("line one\nline\x1b two\u{202e}", false),
            60,
            10,
        );
        assert!(out.contains("line one"), "{out}");
        assert!(out.contains("line two"), "{out}");
        assert!(!out.contains('\x1b') && !out.contains('\u{202e}'));
        let one = out.lines().position(|l| l.contains("line one")).unwrap();
        let two = out.lines().position(|l| l.contains("line two")).unwrap();
        assert!(two > one);
    }

    #[test]
    fn a_payload_that_is_missing_says_so() {
        let mut app = task_app();
        let id = match &app.screen {
            Screen::Task(id) => id.clone(),
            s => panic!("{s:?}"),
        };
        app.handle_key(KeyCode::Char('p'));
        app.apply_frame(Frame {
            payload: Some((id, 3, None)),
            ..good(app.snapshot.clone().unwrap())
        });
        let out = screen(&app, 100, 10);
        assert!(out.contains("no payload"), "{out}");
    }

    #[test]
    fn tiny_and_huge_areas_do_not_panic_on_any_screen() {
        let apps = [
            app_with(vec![], vec![]),
            app_with(vec![task("g", 1)], vec![]),
            {
                let mut a = app_with(vec![], vec![workflow("w", 1, vec![step(0, "s", None)])]);
                a.handle_key(KeyCode::Enter);
                a
            },
            task_app(),
            payload_app("text", true),
        ];
        for app in &apps {
            for (w, h) in [
                (1, 1),
                (20, 5),
                (200, 60),
                (0, 0),
                (1, 40),
                (300, 1),
                (3, 3),
            ] {
                let _ = screen(app, w, h);
            }
        }
    }

    #[test]
    fn six_hundred_items_draw_and_the_selection_stays_visible() {
        let tasks: Vec<_> = (0..600).map(|i| task(&format!("goal-{i:03}"), i)).collect();
        let mut app = app_with(tasks, vec![]);
        for _ in 0..599 {
            app.handle_key(KeyCode::Down);
        }
        let out = screen(&app, 120, 20);
        assert!(out.contains("600 items"), "{out}");
        assert!(
            out.contains("goal-000"),
            "the oldest is last and selected\n{out}"
        );
        let top = screen(
            &app_with(
                (0..600).map(|i| task(&format!("goal-{i:03}"), i)).collect(),
                vec![],
            ),
            120,
            20,
        );
        assert!(top.contains("goal-599"), "{top}");
    }

    #[test]
    fn three_hundred_wide_characters_fit_within_the_area() {
        let goal = "日".repeat(300);
        let mut app = app_with(vec![task(&goal, 1)], vec![]);
        for (w, h) in [(60u16, 8u16), (20, 5), (101, 8)] {
            let out = screen(&app, w, h);
            assert_eq!(out.lines().count(), h as usize);
            for line in out.lines() {
                assert!(line.chars().count() <= w as usize, "{line}");
            }
        }
        app.handle_key(KeyCode::Enter);
        let id = match &app.screen {
            Screen::Task(id) => id.clone(),
            s => panic!("{s:?}"),
        };
        app.apply_frame(Frame {
            contract: Some((id.clone(), contract(&goal))),
            events: Some(EventUpdate::Replace {
                task: id,
                rows: events(1..=2),
                total: 2,
            }),
            ..good(app.snapshot.clone().unwrap())
        });
        for (w, h) in [(60u16, 15u16), (7, 15)] {
            let out = screen(&app, w, h);
            for line in out.lines() {
                assert!(line.chars().count() <= w as usize, "{line}");
            }
        }
    }

    #[test]
    fn wide_characters_never_straddle_cells_and_the_cut_is_visible() {
        let goal = "日".repeat(300);
        let app = app_with(vec![task(&goal, 1)], vec![]);
        for w in [61u16, 100, 600] {
            let buf = buffer(&app, w, 4);
            let mut seen = 0;
            for y in 0..4 {
                let mut x = 0;
                while x < w {
                    if buf[(x, y)].symbol() == "日" {
                        assert!(x + 1 < w, "a wide char straddles the right edge at {w}");
                        assert_ne!(buf[(x + 1, y)].symbol(), "日", "overlapping wide cells");
                        seen += 1;
                        x += 2;
                    } else {
                        x += 1;
                    }
                }
            }
            assert!(seen > 0, "the goal is shown at width {w}");
            assert!(
                seen <= 199,
                "clean cut the goal at 199 chars plus an ellipsis"
            );
        }
        let wide = buffer(&app, 600, 4);
        let mut row = String::new();
        let mut x = 0;
        while x < 600 {
            let symbol = wide[(x, 2)].symbol();
            row.push_str(symbol);
            x += if symbol == "日" { 2 } else { 1 };
        }
        assert_eq!(row.matches('日').count(), 199, "{row}");
        assert!(row.contains("日…"), "the cut ends with an ellipsis\n{row}");
    }

    #[test]
    fn stale_survives_a_narrow_header_and_the_cut_note_goes_first() {
        let mut app = app_with(vec![task("a", 1)], vec![]);
        app.snapshot.as_mut().unwrap().more = true;
        app.stale = Some("database is locked".into());
        let out = screen(&app, 60, 3);
        let header = out.lines().next().unwrap();
        assert!(header.contains("STALE"), "{header}");
        assert!(!header.contains("showing newest"), "{header}");
    }

    #[test]
    fn at_80_columns_a_30_character_goal_is_shown_in_full() {
        let goal = "0123456789abcdefghijklmnopqrst";
        assert_eq!(goal.len(), 30);
        let out = screen(
            &app_with(vec![task(goal, 1), task("other", 2)], vec![]),
            80,
            6,
        );
        assert!(out.contains(goal), "{out}");
        // a cancelling task widens STATE; the row still draws, NAME just gets less
        let mut t = task(goal, 1);
        t.cancel_requested = true;
        let out = screen(&app_with(vec![t], vec![]), 80, 6);
        assert!(out.contains("RUNNING (cancelling)"), "{out}");
    }

    fn payload_rows(app: &App, w: u16, h: u16) -> Vec<String> {
        screen(app, w, h).lines().map(str::to_string).collect()
    }

    #[test]
    fn a_json_payload_is_pretty_printed_over_several_lines() {
        let out = screen(
            &payload_app(r#"{"first":1,"second":{"x":true}}"#, false),
            60,
            12,
        );
        let row = |needle: &str| out.lines().position(|l| l.contains(needle)).unwrap();
        assert!(row("\"first\": 1") < row("\"second\": {"), "{out}");
        assert!(row("\"second\": {") < row("\"x\": true"), "{out}");
    }

    #[test]
    fn a_long_payload_scrolls_with_the_arrow_and_page_keys() {
        let body: Vec<String> = (0..40).map(|i| format!("line-{i:02}")).collect();
        let mut app = payload_app(&body.join("\n"), false);
        let rows = payload_rows(&app, 40, 10);
        assert!(rows.iter().any(|r| r.contains("line-00")), "{rows:?}");
        assert!(!rows.iter().any(|r| r.contains("line-20")), "{rows:?}");
        for _ in 0..3 {
            app.handle_key(KeyCode::Down);
        }
        app.handle_key(KeyCode::Char('j'));
        let rows = payload_rows(&app, 40, 10);
        assert!(rows.iter().any(|r| r.contains("line-04")), "{rows:?}");
        assert!(!rows.iter().any(|r| r.contains("line-03")), "{rows:?}");
        app.handle_key(KeyCode::Up);
        app.handle_key(KeyCode::Char('k'));
        let rows = payload_rows(&app, 40, 10);
        assert!(rows.iter().any(|r| r.contains("line-02")), "{rows:?}");
        // a page is the payload's inner height: 10 rows less header, footer and borders
        app.handle_key(KeyCode::PageDown);
        let rows = payload_rows(&app, 40, 10);
        assert!(rows.iter().any(|r| r.contains("line-08")), "{rows:?}");
        for _ in 0..100 {
            app.handle_key(KeyCode::PageDown);
        }
        let rows = payload_rows(&app, 40, 10);
        assert!(
            rows.iter().any(|r| r.contains("line-39")),
            "the end is reachable\n{rows:?}"
        );
        app.handle_key(KeyCode::Up);
        let rows = payload_rows(&app, 40, 10);
        assert!(
            rows.iter().any(|r| r.contains("line-33")),
            "one Up moves back at once\n{rows:?}"
        );
        for _ in 0..100 {
            app.handle_key(KeyCode::PageUp);
        }
        let rows = payload_rows(&app, 40, 10);
        assert!(rows.iter().any(|r| r.contains("line-00")), "{rows:?}");
        assert!(
            rows.last().unwrap().contains("PgUp"),
            "the footer names the scroll keys"
        );
    }

    #[test]
    fn a_wrapped_single_line_payload_scrolls_too() {
        let mut app = payload_app(&format!("{}END", "x".repeat(2000)), false);
        assert!(!screen(&app, 40, 10).contains("END"));
        for _ in 0..200 {
            app.handle_key(KeyCode::Down);
        }
        assert!(screen(&app, 40, 10).contains("END"));
    }

    #[test]
    fn a_new_payload_opens_at_its_top() {
        let body: Vec<String> = (0..40).map(|i| format!("line-{i:02}")).collect();
        let mut app = payload_app(&body.join("\n"), false);
        screen(&app, 40, 10);
        for _ in 0..10 {
            app.handle_key(KeyCode::Down);
        }
        assert!(!screen(&app, 40, 10).contains("line-00"));
        app.handle_key(KeyCode::Esc);
        app.handle_key(KeyCode::Char('p'));
        let id = match &app.screen {
            Screen::Task(id) => id.clone(),
            s => panic!("{s:?}"),
        };
        app.apply_frame(Frame {
            payload: Some((id, 3, Some((body.join("\n"), false)))),
            ..good(app.snapshot.clone().unwrap())
        });
        assert!(screen(&app, 40, 10).contains("line-00"));
    }

    #[test]
    fn before_the_first_read_the_list_says_loading_and_a_failed_first_read_says_why() {
        let app = App::new();
        let out = screen(&app, 100, 6);
        assert!(out.contains("loading…"), "{out}");
        assert!(!out.contains("no tasks or workflows yet"), "{out}");
        let mut app = App::new();
        app.apply_frame(Frame {
            snapshot: Err("database is locked".into()),
            now: 1,
            events: None,
            contract: None,
            payload: None,
        });
        let out = screen(&app, 100, 6);
        assert!(!out.contains("no tasks or workflows yet"), "{out}");
        let body: Vec<&str> = out.lines().skip(1).collect();
        assert!(
            body.iter().any(|l| l.contains("database is locked")),
            "{out}"
        );
    }

    #[test]
    fn the_header_counts_unreadable_rows() {
        let mut app = app_with(vec![task("a", 1)], vec![]);
        app.snapshot.as_mut().unwrap().skipped = 3;
        let out = screen(&app, 120, 5);
        assert!(out.contains("3 unreadable rows skipped"), "{out}");
    }

    #[test]
    fn the_header_says_how_many_items_the_cut_shows() {
        let mut app = app_with(
            vec![task("a", 1), task("b", 2)],
            vec![workflow("w", 3, vec![step(0, "s", None)])],
        );
        app.snapshot.as_mut().unwrap().more = true;
        let out = screen(&app, 120, 5);
        assert!(out.contains("showing newest 3"), "{out}");
    }

    #[test]
    fn the_task_detail_shows_the_whole_contract() {
        let out = screen(&task_app(), 140, 20);
        for want in [
            "repository: /repo @ main",
            "editable: src",
            "verification: cargo",
            "max_output_tokens_per_request 100",
            "worker_vcpus 1",
            "worker_memory_mib 512",
        ] {
            assert!(out.contains(want), "missing {want}\n{out}");
        }
    }

    #[test]
    fn a_cancelling_workflow_says_so_in_its_detail() {
        let t = task("g", 1);
        let mut wf = workflow("chain", 5, vec![step(0, "alpha", Some(&t))]);
        wf.approved = true;
        wf.cancel_requested = true;
        let mut app = app_with(vec![], vec![wf]);
        assert!(screen(&app, 100, 6).contains("RUNNING (cancelling)"));
        app.handle_key(KeyCode::Enter);
        assert!(screen(&app, 100, 6).contains("RUNNING (cancelling)"));
    }
}
