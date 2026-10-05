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
        .route("/tasks", get(tasks))
        .route("/tasks/{id}", get(task))
        .route("/tasks/{id}/status", get(status))
        .route("/tasks/{id}/result", get(result))
        .route("/tasks/{id}/events", get(events))
        .route("/tasks/{id}/export", post(export))
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
    render(TaskPage {
        active: views::active(&detail.status),
        contract: serde_json::to_string_pretty(&detail.contract)?,
        detail: &detail,
        csrf: &session.csrf,
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
