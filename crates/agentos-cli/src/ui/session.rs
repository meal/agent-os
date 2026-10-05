use super::{
    UiState,
    error::{forbidden, invalid},
};
use crate::app::{AppError, AppErrorKind, AppResult};
use axum::{
    Form,
    body::{Body, to_bytes},
    extract::{FromRequest, State},
    http::{HeaderValue, Method, Request, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct SessionId(pub String);
#[derive(Clone)]
pub(super) struct Session {
    pub id: SessionId,
    pub csrf: String,
}
pub(super) struct Sessions {
    pub launch: String,
    values: Mutex<HashMap<String, Session>>,
}
pub(super) fn random() -> AppResult<String> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes)
        .map_err(|_| AppError::new(AppErrorKind::Unavailable, "Random source unavailable"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}
impl Sessions {
    pub fn new() -> AppResult<Self> {
        Ok(Self {
            launch: random()?,
            values: Mutex::new(HashMap::new()),
        })
    }
    pub fn bootstrap(&self, token: &str) -> AppResult<Session> {
        if token != self.launch {
            return Err(forbidden());
        }
        let mut values = self.values.lock().unwrap();
        if values.len() >= 256 {
            return Err(AppError::new(
                AppErrorKind::Unavailable,
                "Session limit reached; restart the UI.",
            ));
        }
        let id = SessionId(random()?);
        let session = Session {
            id: id.clone(),
            csrf: random()?,
        };
        values.insert(id.0, session.clone());
        Ok(session)
    }
    fn cookie(&self, request: &Request<Body>) -> Option<Session> {
        let value = request.headers().get(header::COOKIE)?.to_str().ok()?;
        let cookie = value
            .split(';')
            .filter_map(|p| p.trim().split_once('='))
            .find(|(k, _)| *k == "agentos_session")?
            .1;
        self.values.lock().unwrap().get(cookie).cloned()
    }
}
#[derive(serde::Deserialize)]
struct CsrfForm {
    #[serde(rename = "_csrf")]
    csrf: String,
}
pub(super) async fn boundary(
    State(state): State<Arc<UiState>>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let result = async {
        if request.headers().get_all(header::HOST).iter().count() != 1
            || request
                .headers()
                .get(header::HOST)
                .and_then(|h| h.to_str().ok())
                != Some(&state.authority)
        {
            return Err(forbidden());
        }
        let path = request.uri().path().to_owned();
        let public =
            request.method() == Method::GET && (path == "/" || path.starts_with("/assets/"));
        let bootstrap = request.method() == Method::POST && path == "/session";
        let session = if public || bootstrap {
            None
        } else {
            Some(state.sessions.cookie(&request).ok_or_else(forbidden)?)
        };
        if request.method() == Method::POST {
            if request.headers().get_all(header::ORIGIN).iter().count() != 1
                || request
                    .headers()
                    .get(header::ORIGIN)
                    .and_then(|h| h.to_str().ok())
                    != Some(&state.origin)
            {
                return Err(forbidden());
            }
            let (parts, body) = request.into_parts();
            let bytes = to_bytes(body, 256 * 1024).await.map_err(|_| {
                AppError::new(AppErrorKind::TooLarge, "Request body exceeds 256 KiB.")
            })?;
            request = Request::from_parts(parts, Body::from(bytes.clone()));
            if let Some(s) = &session {
                let token = if let Some(h) = request.headers().get("x-csrf-token") {
                    h.to_str()
                        .map(str::to_owned)
                        .map_err(|_| invalid("Invalid CSRF header"))?
                } else {
                    let mut copy = Request::new(Body::from(bytes));
                    *copy.method_mut() = request.method().clone();
                    *copy.headers_mut() = request.headers().clone();
                    Form::<CsrfForm>::from_request(copy, &())
                        .await
                        .map_err(|_| forbidden())?
                        .0
                        .csrf
                };
                if token != s.csrf {
                    return Err(forbidden());
                }
            }
        } else if !matches!(*request.method(), Method::GET | Method::HEAD) {
            return Err(invalid("Unsupported request method"));
        }
        if let Some(session) = session {
            request.extensions_mut().insert(session);
        }
        Ok(next.run(request).await)
    }
    .await;
    let mut response = result.unwrap_or_else(IntoResponse::into_response);
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::CONTENT_SECURITY_POLICY,HeaderValue::from_static("default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    response
}
