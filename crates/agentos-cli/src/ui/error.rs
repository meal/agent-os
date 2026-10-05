use crate::app::{AppError, AppErrorKind};
use askama::Template;
use axum::{
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};
#[derive(Template)]
#[template(path = "ui/error.html")]
struct ErrorPage<'a> {
    message: &'a str,
}
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match self.kind {
            AppErrorKind::Invalid => StatusCode::BAD_REQUEST,
            AppErrorKind::Forbidden => StatusCode::FORBIDDEN,
            AppErrorKind::NotFound => StatusCode::NOT_FOUND,
            AppErrorKind::Conflict => StatusCode::CONFLICT,
            AppErrorKind::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            AppErrorKind::Gone => StatusCode::GONE,
            AppErrorKind::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        };
        (
            status,
            Html(
                ErrorPage {
                    message: &self.message,
                }
                .render()
                .unwrap_or_else(|_| "Request unavailable".into()),
            ),
        )
            .into_response()
    }
}
pub(super) fn invalid(message: &str) -> AppError {
    AppError::new(AppErrorKind::Invalid, message)
}
pub(super) fn forbidden() -> AppError {
    AppError::new(
        AppErrorKind::Forbidden,
        "Open the launch link printed by agentos ui, or refresh this page before trying again.",
    )
}
