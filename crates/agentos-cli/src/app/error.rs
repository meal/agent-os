use crate::error::CliError;
use agentos_engine::export::ExportError;
use agentos_store::db::DbError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // HTTP/download adapters consume the remaining categories.
pub(crate) enum AppErrorKind {
    Invalid,
    Forbidden,
    NotFound,
    Conflict,
    TooLarge,
    Gone,
    Unavailable,
}
#[derive(Debug, Clone)]
pub(crate) struct AppError {
    #[allow(dead_code)] // Read by the following HTTP adapter increment.
    pub kind: AppErrorKind,
    pub cli_code: i32,
    pub message: String,
}
pub(crate) type AppResult<T> = Result<T, AppError>;
impl AppError {
    pub(crate) fn new(kind: AppErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            cli_code: if kind == AppErrorKind::Invalid { 2 } else { 1 },
            message: message.into(),
        }
    }
}
impl From<CliError> for AppError {
    fn from(e: CliError) -> Self {
        Self {
            kind: if e.code == 2 {
                AppErrorKind::Invalid
            } else {
                AppErrorKind::Unavailable
            },
            cli_code: e.code,
            message: e.message,
        }
    }
}
impl From<AppError> for CliError {
    fn from(e: AppError) -> Self {
        Self {
            code: e.cli_code,
            message: e.message,
        }
    }
}
impl From<DbError> for AppError {
    fn from(e: DbError) -> Self {
        let kind = match &e {
            DbError::NotFound(_) | DbError::EffectNotFound(_) => AppErrorKind::NotFound,
            DbError::InvalidQuery(_) => AppErrorKind::Invalid,
            DbError::ReadLimit { .. } => AppErrorKind::TooLarge,
            DbError::CapabilityDenied { .. } => AppErrorKind::Forbidden,
            DbError::Transition(_) | DbError::VersionConflict { .. } => AppErrorKind::Conflict,
            _ => AppErrorKind::Unavailable,
        };
        let cli: CliError = e.into();
        Self {
            kind,
            cli_code: cli.code,
            message: cli.message,
        }
    }
}
impl From<ExportError> for AppError {
    fn from(e: ExportError) -> Self {
        if let ExportError::Db(e) = e {
            return e.into();
        }
        let kind = match &e {
            ExportError::NotTerminal(_) => AppErrorKind::Conflict,
            ExportError::ReadLimit { .. } => AppErrorKind::TooLarge,
            _ => AppErrorKind::Unavailable,
        };
        let cli: CliError = e.into();
        Self {
            kind,
            cli_code: cli.code,
            message: cli.message,
        }
    }
}
impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        Self::new(AppErrorKind::Unavailable, e.to_string())
    }
}
impl From<serde_json::Error> for AppError {
    fn from(e: serde_json::Error) -> Self {
        Self::new(AppErrorKind::Unavailable, e.to_string())
    }
}
