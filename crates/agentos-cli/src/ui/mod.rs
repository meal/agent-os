mod error;
mod routes;
mod session;
mod views;
use crate::{
    app::{AppError, AppErrorKind, AppResult},
    error::CliError,
    home::Home,
};
use axum::{Router, middleware};
use std::sync::Arc;
use tokio::sync::Semaphore;
pub(crate) struct UiConfig {
    pub port: u16,
}
pub(crate) struct UiState {
    pub home: Arc<Home>,
    pub queries: Arc<Semaphore>,
    sessions: session::Sessions,
    pub authority: String,
    pub origin: String,
}
impl UiState {
    async fn query<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Home) -> AppResult<T> + Send + 'static,
    ) -> AppResult<T> {
        let permit = self.queries.clone().try_acquire_owned().map_err(|_| {
            AppError::new(
                AppErrorKind::Unavailable,
                "The read service is busy. Try again.",
            )
        })?;
        let home = self.home.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            f(&home)
        })
        .await
        .map_err(|_| AppError::new(AppErrorKind::Unavailable, "Read operation failed"))?
    }
}
pub(crate) fn router(state: Arc<UiState>) -> Router {
    routes::router()
        .layer(middleware::from_fn_with_state(
            state.clone(),
            session::boundary,
        ))
        .with_state(state)
}
pub(crate) async fn serve(home: Home, config: UiConfig) -> Result<(), CliError> {
    home.open()?;
    let listener =
        tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, config.port)).await?;
    let authority = listener.local_addr()?.to_string();
    let origin = format!("http://{authority}");
    let sessions = session::Sessions::new()?;
    println!(
        "{}",
        serde_json::json!({"listening":origin,"launch_url":format!("{origin}/#{}",sessions.launch)})
    );
    let state = Arc::new(UiState {
        home: Arc::new(home),
        queries: Arc::new(Semaphore::new(4)),
        sessions,
        authority,
        origin,
    });
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}
async fn shutdown() {
    #[cfg(unix)]
    {
        let mut signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! {_=tokio::signal::ctrl_c()=>{},_=signal.recv()=>{}}
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
