//! CLI presentation around shared immutable submission and task driving.
use super::{print, print_state};
use crate::{
    app::{
        control::{self, RunRequest},
        submission::{self, CreateRequest},
    },
    args::WorkerKind,
    crash::CrashSpec,
    error::CliError,
    home::Home,
};
use std::path::Path;
pub async fn submit(
    home: &Home,
    task_file: &Path,
    yes: bool,
    patch: Option<&Path>,
    model: Option<&str>,
    crash: Option<&CrashSpec>,
) -> Result<(), CliError> {
    let text = std::fs::read_to_string(task_file)
        .map_err(|e| CliError::usage(format!("cannot read {}: {e}", task_file.display())))?;
    let recorded = submission::create_locked(
        home,
        CreateRequest {
            contract_json: text,
            worker: home.worker.unwrap_or(WorkerKind::Host),
            model: model.map(str::to_owned),
            patch: patch.map(Path::to_path_buf),
        },
        yes,
    )?;
    eprint!("{}", recorded.description);
    match recorded.lock {
        Some(lock) => {
            let prepared = control::prepare_locked(
                home,
                RunRequest {
                    task: recorded.submission.task_id,
                    patch: None,
                    crash: crash.cloned(),
                    reviewed_contract: None,
                },
                lock,
            )?;
            let result = control::drive_prepared(prepared).await?;
            print_state(&result.task_id, result.state);
        }
        None => print(&recorded.submission.summary),
    };
    Ok(())
}
