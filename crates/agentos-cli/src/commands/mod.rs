mod component;
mod control;
mod export;
mod gc;
mod host_check;
mod image;
mod inspect;
mod profile;
pub(crate) mod registry;
mod revoke;
mod submit;
pub mod supervise;

use agentos_core::ids::TaskId;
use agentos_core::state::TaskState;
use serde_json::{Value, json};

use crate::args::{Args, Command, ComponentCommand, ImageCommand, ProfileCommand};
use crate::error::CliError;
use crate::home::Home;

pub async fn dispatch(args: Args) -> Result<(), CliError> {
    let home = Home {
        worker: args.worker,
        firecracker: args.firecracker,
        jailer: args.jailer,
        jail_uid: args.jail_uid,
        jail_gid: args.jail_gid,
        allow_unjailed: args.allow_unjailed,
        api_key_file: args.api_key_file,
        anthropic_base_url: args.anthropic_base_url,
        ..Home::new(args.home, args.profiles)?
    };
    // Every command on an existing task follows the worker recorded at its submission; a
    // `--worker` naming another one is refused before anything else happens.
    if home.worker.is_some()
        && let Some(id) = task_arg(&args.command)
    {
        home.task_worker(&home.open()?, &task_id(id)?)?;
    }
    match args.command {
        Command::Submit {
            task,
            yes,
            fake_agent_patch,
            model,
            crash_at,
            agent_cli,
        } => {
            submit::submit(
                &home,
                &task,
                yes,
                fake_agent_patch.as_deref(),
                model.as_deref(),
                agent_cli,
                crash_at.as_ref(),
            )
            .await
        }
        Command::Status { id } => inspect::status(&home, &task_id(&id)?),
        Command::Events { id } => inspect::events(&home, &task_id(&id)?),
        Command::Pause { id } => control::pause(&home, &task_id(&id)?),
        Command::Resume {
            id,
            fake_agent_patch,
            crash_at,
        } => {
            control::resume(
                &home,
                &task_id(&id)?,
                fake_agent_patch.as_deref(),
                crash_at.as_ref(),
            )
            .await
        }
        Command::Cancel { id } => control::cancel(&home, &task_id(&id)?).await,
        Command::Profile {
            command: ProfileCommand::Register { dir },
        } => profile::register(&home, &dir),
        Command::Profile {
            command: ProfileCommand::List,
        } => profile::list(&home),
        Command::Image {
            command: ImageCommand::Register { dir },
        } => image::register(&home, &dir),
        Command::Image {
            command: ImageCommand::List,
        } => image::list(&home),
        Command::Version => host_check::version(),
        Command::HostCheck => host_check::host_check(&home),
        Command::Component {
            command: ComponentCommand::Register { dir },
        } => component::register(&home, &dir),
        Command::Component {
            command: ComponentCommand::List,
        } => component::list(&home),
        Command::Revoke { id, capability } => {
            revoke::revoke(&home, &task_id(&id)?, capability.as_deref())
        }
        Command::Export { id, dir } => export::export(&home, &task_id(&id)?, &dir),
        Command::Gc {
            dry_run,
            batch_size,
        } => gc::gc(&home, dry_run, batch_size as usize),
        Command::Supervise { .. } => unreachable!("handled before the runtime starts"),
    }
}

/// The task id a command acts on, if any.
fn task_arg(command: &Command) -> Option<&str> {
    match command {
        Command::Status { id }
        | Command::Events { id }
        | Command::Pause { id }
        | Command::Resume { id, .. }
        | Command::Cancel { id }
        | Command::Revoke { id, .. }
        | Command::Export { id, .. } => Some(id),
        _ => None,
    }
}

/// Task ids are UUIDs; anything else is refused before it can reach a path.
fn task_id(id: &str) -> Result<TaskId, CliError> {
    let uuid_shaped = id.len() == 36
        && id.chars().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        });
    if !uuid_shaped {
        return Err(CliError::usage(format!(
            "invalid task id {id:?}: expected a UUID"
        )));
    }
    serde_json::from_value(Value::String(id.to_string()))
        .map_err(|e| CliError::usage(format!("invalid task id {id:?}: {e}")))
}

/// One JSON document on stdout.
fn print(v: &Value) {
    println!("{v}");
}

fn print_state(task: &TaskId, state: TaskState) {
    print(&json!({ "task_id": task, "state": state.label() }));
}

/// See [`submit::tls_ready`].
pub(crate) fn submit_tls_ready() -> Result<(), crate::error::CliError> {
    submit::tls_ready()
}
