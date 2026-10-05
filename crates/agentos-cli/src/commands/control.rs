use super::{print, print_state};
use crate::{
    app::control::{self, RunRequest},
    crash::CrashSpec,
    error::CliError,
    home::Home,
};
use agentos_core::ids::TaskId;
use serde_json::json;
use std::path::Path;
pub fn pause(home: &Home, task: &TaskId) -> Result<(), CliError> {
    let r = control::pause(home, task)?;
    print_state(&r.task_id, r.state);
    Ok(())
}
pub async fn resume(
    home: &Home,
    task: &TaskId,
    patch: Option<&Path>,
    crash: Option<&CrashSpec>,
) -> Result<(), CliError> {
    let p = control::prepare(
        home,
        RunRequest {
            task: task.clone(),
            patch: patch.map(Path::to_path_buf),
            crash: crash.cloned(),
            reviewed_contract: None,
        },
    )?;
    let r = control::drive_prepared(p).await?;
    print_state(&r.task_id, r.state);
    Ok(())
}
pub async fn cancel(home: &Home, task: &TaskId) -> Result<(), CliError> {
    let r = control::cancel(home, task).await?;
    match r.note {
        Some(note) if r.state.is_terminal() => {
            print(&json!({"task_id":r.task_id,"state":r.state.label(),"note":note}))
        }
        Some(note) => print(
            &json!({"task_id":r.task_id,"state":r.state.label(),"cancel_requested":r.cancel_requested,"note":note}),
        ),
        None => print_state(&r.task_id, r.state),
    };
    Ok(())
}
