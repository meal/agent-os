use super::views::{self, EventsPage, ResultView, StatusPage, TaskPage};
use super::{UiState, error::invalid, session::Session};
use crate::app::{AppError, AppErrorKind, AppResult, queries, types::TaskSummary};
use agentos_core::state::TaskState;
use agentos_store::read::TaskCursor;
use askama::Template;
use axum::{
    Json, Router,
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{HeaderValue, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use std::sync::Arc;
#[derive(Template)]
#[template(path = "ui/bootstrap.html")]
struct Bootstrap;
#[derive(Template)]
#[template(path = "ui/tasks.html")]
struct Tasks<'a> {
    home: &'a str,
    rows: &'a [TaskSummary],
    next: String,
    filter: String,
}
#[derive(serde::Deserialize)]
struct BootstrapInput {
    token: String,
}
#[derive(serde::Deserialize, Default)]
struct ListInput {
    filter: Option<String>,
    cursor: Option<String>,
}
pub(super) fn router() -> Router<Arc<UiState>> {
    Router::new()
        .route("/", get(|| async { Html(Bootstrap.render().unwrap()) }))
        .route("/session", post(bootstrap))
        .route("/assets/{*path}", get(asset))
        .route("/tasks", get(tasks).post(create))
        .route("/tasks/new", get(new_task))
        .route("/tasks/{id}", get(task))
        .route("/tasks/{id}/status", get(status))
        .route("/tasks/{id}/result", get(result))
        .route("/tasks/{id}/events", get(events))
        .route("/tasks/{id}/export", post(export))
        .route("/tasks/{id}/start", post(start))
        .route("/tasks/{id}/resume", post(resume))
        .route("/tasks/{id}/pause", post(pause))
        .route("/tasks/{id}/cancel", post(cancel))
        .route("/tasks/{id}/actions", get(actions))
        .route("/downloads/{id}", get(download))
        .fallback(|| async { AppError::new(AppErrorKind::NotFound, "Page not found") })
}
async fn bootstrap(
    State(state): State<Arc<UiState>>,
    input: Result<Json<BootstrapInput>, JsonRejection>,
) -> AppResult<Response> {
    let Json(input) = input.map_err(|_| invalid("Invalid launch request"))?;
    let Session { id, csrf } = state.sessions.bootstrap(&input.token)?;
    let mut response = Json(serde_json::json!({"csrf":csrf})).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "agentos_session={}; HttpOnly; SameSite=Strict; Path=/",
            id.0
        ))
        .unwrap(),
    );
    Ok(response)
}
async fn asset(Path(path): Path<String>) -> AppResult<Response> {
    let (kind, body) = match path.as_str() {
        "htmx.min.js" => (
            "text/javascript; charset=utf-8",
            include_str!("../../assets/ui/htmx.min.js"),
        ),
        "bootstrap.js" => (
            "text/javascript; charset=utf-8",
            include_str!("../../assets/ui/bootstrap.js"),
        ),
        "app.css" => (
            "text/css; charset=utf-8",
            include_str!("../../assets/ui/app.css"),
        ),
        "app.js" => (
            "text/javascript; charset=utf-8",
            include_str!("../../assets/ui/app.js"),
        ),
        _ => return Err(AppError::new(AppErrorKind::NotFound, "Asset not found")),
    };
    Ok(([(header::CONTENT_TYPE, kind)], body).into_response())
}
async fn tasks(
    State(state): State<Arc<UiState>>,
    input: Result<Query<ListInput>, QueryRejection>,
) -> AppResult<Html<String>> {
    let Query(input) = input.map_err(|_| invalid("Invalid task filters"))?;
    let filter = match input.filter.as_deref().filter(|s| !s.is_empty()) {
        None => None,
        Some(value) => Some(
            [
                TaskState::Ready,
                TaskState::Running,
                TaskState::Waiting,
                TaskState::Paused,
                TaskState::Verifying,
                TaskState::Succeeded,
                TaskState::Failed,
                TaskState::Cancelled,
            ]
            .into_iter()
            .find(|s| s.label() == value)
            .ok_or_else(|| invalid("Unknown task state"))?,
        ),
    };
    let cursor = match input.cursor {
        None => None,
        Some(c) if c.len() <= 512 => Some(
            serde_json::from_slice::<TaskCursor>(
                &URL_SAFE_NO_PAD
                    .decode(c)
                    .map_err(|_| invalid("Invalid page cursor"))?,
            )
            .map_err(|_| invalid("Invalid page cursor"))?,
        ),
        _ => return Err(invalid("Invalid page cursor")),
    };
    let page = state
        .query(move |home| queries::tasks(home, filter, cursor.as_ref()))
        .await?;
    let next = page
        .next
        .map(|c| URL_SAFE_NO_PAD.encode(serde_json::to_vec(&c).unwrap()))
        .unwrap_or_default();
    Ok(Html(
        Tasks {
            home: &state.home.root.display().to_string(),
            rows: &page.rows,
            next,
            filter: input.filter.unwrap_or_default(),
        }
        .render()
        .map_err(|_| AppError::new(AppErrorKind::Unavailable, "Could not render tasks"))?,
    ))
}

fn task_id(id: String) -> AppResult<agentos_core::ids::TaskId> {
    crate::commands::task_id(&id).map_err(|_| invalid("Invalid task ID"))
}
fn render<T: Template>(value: T) -> AppResult<Html<String>> {
    Ok(Html(value.render().map_err(|_| {
        AppError::new(AppErrorKind::Unavailable, "Could not render task view")
    })?))
}
async fn task(
    State(state): State<Arc<UiState>>,
    axum::Extension(session): axum::Extension<Session>,
    Path(id): Path<String>,
) -> AppResult<Html<String>> {
    let id = task_id(id)?;
    let detail = state.query(move |home| queries::detail(home, &id)).await?;
    let actions = action_view(&state, &session, &detail).await?;
    render(TaskPage {
        active: views::active(&detail.status),
        contract: serde_json::to_string_pretty(&detail.contract)?,
        detail: &detail,
        csrf: &session.csrf,
        actions,
    })
}
async fn status(
    State(state): State<Arc<UiState>>,
    Path(id): Path<String>,
) -> AppResult<Html<String>> {
    let id = task_id(id)?;
    let status = state
        .query(move |home| queries::status(home, &id, Some(queries::RESULT_LIMIT)))
        .await?;
    render(StatusPage {
        active: views::active(&status),
        status: &status,
    })
}
async fn result(
    State(state): State<Arc<UiState>>,
    Path(id): Path<String>,
) -> AppResult<Html<String>> {
    let id = task_id(id)?;
    let result: ResultView = state
        .query(move |home| queries::review(home, &id))
        .await?
        .into();
    Ok(Html(views::render_result(&result).map_err(|_| {
        AppError::new(AppErrorKind::Unavailable, "Could not render result")
    })?))
}
#[derive(serde::Deserialize, Default)]
struct EventInput {
    #[serde(default)]
    after: u64,
}
async fn events(
    State(state): State<Arc<UiState>>,
    Path(id): Path<String>,
    input: Result<Query<EventInput>, QueryRejection>,
) -> AppResult<Html<String>> {
    let id = task_id(id)?;
    let Query(input) = input.map_err(|_| invalid("Invalid event sequence"))?;
    let page = state
        .query(move |home| queries::events(home, &id, input.after))
        .await?;
    render(EventsPage { page: &page })
}

#[derive(Template)]
#[template(path = "ui/export.html")]
struct ExportPage {
    path: String,
    bytes: u64,
    task: String,
}
async fn export(
    State(state): State<Arc<UiState>>,
    axum::Extension(session): axum::Extension<Session>,
    Path(id): Path<String>,
) -> AppResult<Html<String>> {
    let id = task_id(id)?;
    let cache = state.downloads.clone();
    let ticket = state
        .query(move |home| cache.create(&session.id, home, &id))
        .await?;
    render(ExportPage {
        path: format!("/downloads/{}", ticket.id.0),
        bytes: ticket.bytes,
        task: ticket.task.to_string(),
    })
}
async fn download(
    State(state): State<Arc<UiState>>,
    axum::Extension(session): axum::Extension<Session>,
    Path(id): Path<String>,
) -> AppResult<Response> {
    let permit = state
        .streams
        .clone()
        .try_acquire_owned()
        .map_err(|_| AppError::new(AppErrorKind::Unavailable, "Download streams are busy"))?;
    let cache = state.downloads.clone();
    let mut reader = state
        .query(move |home| cache.open(&session.id, &super::downloads::DownloadId(id), home))
        .await?;
    reader.permit = Some(permit);
    let bytes = reader.bytes;
    let mut response =
        axum::body::Body::from_stream(tokio_util::io::ReaderStream::new(reader)).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-tar"),
    );
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=agentos-export.tar"),
    );
    response.headers_mut().insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&bytes.to_string()).unwrap(),
    );
    Ok(response)
}

#[derive(serde::Deserialize)]
struct StartInput {
    contract_digest: String,
}
fn request(
    id: agentos_core::ids::TaskId,
    reviewed_contract: Option<agentos_core::ids::Digest>,
) -> crate::app::control::RunRequest {
    crate::app::control::RunRequest {
        task: id,
        patch: None,
        crash: None,
        reviewed_contract,
    }
}
async fn admission(
    state: &UiState,
    admitted: crate::app::runner::RunAdmission,
) -> AppResult<Response> {
    let (id, code) = match admitted {
        crate::app::runner::RunAdmission::Started(id) => (id, axum::http::StatusCode::ACCEPTED),
        crate::app::runner::RunAdmission::Reported(r) => (r.task_id, axum::http::StatusCode::OK),
    };
    let status = state
        .query(move |home| queries::status(home, &id, Some(queries::RESULT_LIMIT)))
        .await?;
    Ok((
        code,
        render(StatusPage {
            active: views::active(&status),
            status: &status,
        })?,
    )
        .into_response())
}
async fn start(
    State(state): State<Arc<UiState>>,
    Path(id): Path<String>,
    input: Result<axum::Form<StartInput>, axum::extract::rejection::FormRejection>,
) -> AppResult<Response> {
    let id = task_id(id)?;
    let axum::Form(input) = input.map_err(|_| invalid("A reviewed contract digest is required"))?;
    let digest = agentos_core::ids::Digest::from_hex(&input.contract_digest)
        .map_err(|_| invalid("Invalid reviewed digest"))?;
    let admitted = state.runner.admit(request(id, Some(digest))).await?;
    admission(&state, admitted).await
}
async fn resume(State(state): State<Arc<UiState>>, Path(id): Path<String>) -> AppResult<Response> {
    let id = task_id(id)?;
    let check = id.clone();
    let status = state
        .query(move |home| queries::status(home, &check, Some(queries::RESULT_LIMIT)))
        .await?;
    if status.state == "READY" {
        return Err(AppError::new(
            AppErrorKind::Conflict,
            "Review the READY contract and explicitly approve it before starting",
        ));
    }
    let admitted = state.runner.admit(request(id, None)).await?;
    admission(&state, admitted).await
}
async fn pause(State(state): State<Arc<UiState>>, Path(id): Path<String>) -> AppResult<Response> {
    let id = task_id(id)?;
    let result = state
        .runner
        .operation(move |home| crate::app::control::pause(home, &id))?
        .await
        .map_err(|_| {
            AppError::new(
                AppErrorKind::Unavailable,
                "Control operation was interrupted",
            )
        })??;
    let id = result.task_id;
    let status = state
        .query(move |home| queries::status(home, &id, Some(queries::RESULT_LIMIT)))
        .await?;
    Ok(render(StatusPage {
        active: views::active(&status),
        status: &status,
    })?
    .into_response())
}
async fn cancel(State(state): State<Arc<UiState>>, Path(id): Path<String>) -> AppResult<Response> {
    let id = task_id(id)?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    let _completion = state.runner.operation(move |home| {
        crate::app::runner::runtime()?.block_on(crate::app::control::cancel_acknowledged(
            home,
            &id,
            move |result| {
                let _ = tx.send(result);
            },
        ))
    })?;
    let result = rx.await.map_err(|_| {
        AppError::new(
            AppErrorKind::Unavailable,
            "Cancellation could not be admitted; check the recorded state",
        )
    })?;
    let terminal = result.state.is_terminal();
    let id = result.task_id;
    let status = state
        .query(move |home| queries::status(home, &id, Some(queries::RESULT_LIMIT)))
        .await?;
    Ok((
        if terminal {
            axum::http::StatusCode::OK
        } else {
            axum::http::StatusCode::ACCEPTED
        },
        render(StatusPage {
            active: views::active(&status),
            status: &status,
        })?,
    )
        .into_response())
}
#[derive(Template)]
#[template(path = "ui/actions.html")]
struct Actions<'a> {
    id: String,
    state: String,
    csrf: &'a str,
    digest: String,
    busy: bool,
    has_agent: bool,
    outstanding: bool,
    diagnostic: String,
}
async fn actions(
    State(state): State<Arc<UiState>>,
    axum::Extension(session): axum::Extension<Session>,
    Path(id): Path<String>,
) -> AppResult<Html<String>> {
    let id = task_id(id)?;
    let detail = state.query(move |home| queries::detail(home, &id)).await?;
    Ok(Html(action_view(&state, &session, &detail).await?))
}
async fn action_view(
    state: &UiState,
    session: &Session,
    detail: &crate::app::types::TaskDetail,
) -> AppResult<String> {
    let id = detail.status.task_id.clone();
    let (external, patch) = state
        .query(move |home| {
            Ok((
                home.driver_status()?,
                home.task_dir(&id).join(crate::drive::AGENT_PATCH).is_file(),
            ))
        })
        .await?;
    let has_agent = detail.status.model != crate::drive::FAKE_AGENT || patch;
    Ok(render(Actions {
        id: detail.status.task_id.to_string(),
        state: detail.status.state.clone(),
        digest: detail.contract_digest.to_string(),
        csrf: &session.csrf,
        busy: state.runner.busy() || external.is_some(),
        has_agent,
        outstanding: !detail.status.outstanding_effects.is_empty(),
        diagnostic: state.runner.diagnostic().unwrap_or_default(),
    })?
    .0)
}

#[derive(Template)]
#[template(path = "ui/new.html")]
struct NewTask<'a> {
    csrf: &'a str,
    nonce: String,
    home: String,
    profiles_path: String,
    images_path: String,
    registries: String,
    example: String,
}
async fn new_task(
    State(state): State<Arc<UiState>>,
    axum::Extension(session): axum::Extension<Session>,
) -> AppResult<Html<String>> {
    let nonce = state.forms.issue(&session.id)?;
    let registries = state
        .query(|home| {
            Ok(serde_json::to_string_pretty(
                &serde_json::json!({"profiles":home.registry_list().into_iter().map(registry_entry).collect::<Vec<_>>(),"images":home.image_list().into_iter().map(registry_entry).collect::<Vec<_>>()}),
            )?)
        })
        .await?;
    render(NewTask {
        csrf: &session.csrf,
        nonce: nonce.0,
        home: state.home.root.display().to_string(),
        profiles_path: state.home.profiles.display().to_string(),
        images_path: state.home.images_dir().display().to_string(),
        registries,
        example: serde_json::to_string_pretty(
            &serde_json::json!({"goal":"Describe the task", "repository":{"source":"/server/path/repository","revision":"recorded-at-submission"}, "profile":"python-stdlib-v1", "verification_profile":"parser-checks-v1", "editable_paths":["src/**"], "capabilities":["snapshot.read","workspace.apply_patch","verification.run","artifact.export","model.request"], "limits":{"model_requests":12,"max_output_tokens_per_request":4096,"tool_actions":50,"deadline_seconds":600,"worker_vcpus":1,"worker_memory_mib":256}}),
        )?,
    })
}
#[derive(serde::Deserialize)]
struct CreateInput {
    nonce: String,
    contract_json: String,
    model: String,
    worker: String,
}
async fn create(
    State(state): State<Arc<UiState>>,
    axum::Extension(session): axum::Extension<Session>,
    headers: axum::http::HeaderMap,
    input: Result<axum::Form<CreateInput>, axum::extract::rejection::FormRejection>,
) -> AppResult<Response> {
    let axum::Form(input) = input.map_err(|_| invalid("Invalid task form"))?;
    if input.model.trim().is_empty() {
        return Err(invalid("A model spec is required"));
    }
    let worker = match input.worker.as_str() {
        "host" => crate::args::WorkerKind::Host,
        "firecracker" => crate::args::WorkerKind::Firecracker,
        _ => return Err(invalid("Unknown worker")),
    };
    let submission = state
        .forms
        .submit(
            &state.runner,
            &session.id,
            &super::forms::FormNonce(input.nonce),
            crate::app::submission::CreateRequest {
                contract_json: input.contract_json,
                worker,
                model: Some(input.model),
                patch: None,
            },
        )
        .await?;
    let path = format!("/tasks/{}", submission.task_id);
    if headers
        .get("hx-request")
        .is_some_and(|value| value == "true")
    {
        let mut response = Html("Task recorded. Opening review.").into_response();
        response
            .headers_mut()
            .insert("hx-redirect", HeaderValue::from_str(&path).unwrap());
        Ok(response)
    } else {
        Ok(axum::response::Redirect::to(&path).into_response())
    }
}

fn registry_entry(entry: crate::home::RegistryEntry) -> serde_json::Value {
    serde_json::json!({"id":entry.id,"digest":entry.digest,"path":entry.dir,"registered_ms":entry.registered_ms})
}
