mod control;
mod export;
mod image;
mod inspect;
mod profile;
pub(crate) mod registry;
mod revoke;
mod submit;
pub mod supervise;

use agentos_core::ids::TaskId;
use agentos_core::state::TaskState;
use serde_json::{json, Value};

use crate::args::{Args, Command, ImageCommand, ProfileCommand};
use crate::error::CliError;
use crate::home::Home;

pub async fn dispatch(args: Args) -> Result<(), CliError> {
    let home = Home::new(args.home, args.profiles)?;
    match args.command {
        Command::Submit { task, yes, fake_agent_patch, crash_at } => {
            submit::submit(&home, &task, yes, fake_agent_patch.as_deref(), crash_at.as_ref()).await
        }
        Command::Status { id } => inspect::status(&home, &task_id(&id)?),
        Command::Events { id } => inspect::events(&home, &task_id(&id)?),
        Command::Pause { id } => control::pause(&home, &task_id(&id)?),
        Command::Resume { id, fake_agent_patch, crash_at } => {
            control::resume(&home, &task_id(&id)?, fake_agent_patch.as_deref(), crash_at.as_ref()).await
        }
        Command::Cancel { id } => control::cancel(&home, &task_id(&id)?).await,
        Command::Profile { command: ProfileCommand::Register { dir } } => profile::register(&home, &dir),
        Command::Profile { command: ProfileCommand::List } => profile::list(&home),
        Command::Image { command: ImageCommand::Register { dir } } => image::register(&home, &dir),
        Command::Image { command: ImageCommand::List } => image::list(&home),
        Command::Revoke { id, capability } => revoke::revoke(&home, &task_id(&id)?, capability.as_deref()),
        Command::Export { id, dir } => export::export(&home, &task_id(&id)?, &dir),
        Command::Supervise { .. } => unreachable!("handled before the runtime starts"),
    }
}

/// Task ids are UUIDs; anything else is refused before it can reach a path.
fn task_id(id: &str) -> Result<TaskId, CliError> {
    let uuid_shaped = id.len() == 36 && id.chars().enumerate().all(|(i, c)| match i {
        8 | 13 | 18 | 23 => c == '-',
        _ => c.is_ascii_hexdigit(),
    });
    if !uuid_shaped {
        return Err(CliError::usage(format!("invalid task id {id:?}: expected a UUID")));
    }
    serde_json::from_value(Value::String(id.to_string())).map_err(|e| CliError::usage(format!("invalid task id {id:?}: {e}")))
}

/// One JSON document on stdout.
fn print(v: &Value) {
    println!("{v}");
}

fn print_state(task: &TaskId, state: TaskState) {
    print(&json!({ "task_id": task, "state": state.label() }));
}
