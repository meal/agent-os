use super::session::{SessionId, random};
use crate::app::{
    AppError, AppErrorKind, AppResult,
    runner::RunnerManager,
    submission::{self, CreateRequest, Submission},
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::watch;
#[derive(Clone)]
pub(super) struct FormNonce(pub String);
struct Entry {
    expires: u64,
    digest: Option<blake3::Hash>,
    completion: Option<watch::Sender<Option<AppResult<Submission>>>>,
}
impl Entry {
    fn pending(&self) -> bool {
        self.completion
            .as_ref()
            .is_some_and(|tx| tx.borrow().is_none())
    }
}
pub(super) struct SubmissionForms {
    clock: Arc<dyn Fn() -> Duration + Send + Sync>,
    entries: Mutex<HashMap<(SessionId, String), Entry>>,
}
impl SubmissionForms {
    pub fn new(clock: Arc<dyn Fn() -> Duration + Send + Sync>) -> Self {
        Self {
            clock,
            entries: Mutex::new(HashMap::new()),
        }
    }
    pub fn issue(&self, session: &SessionId) -> AppResult<FormNonce> {
        let now = (self.clock)().as_secs();
        let mut entries = self.entries.lock().unwrap();
        entries.retain(|_, e| e.expires > now || e.pending());
        if entries.keys().filter(|(s, _)| s == session).count() >= 256 {
            return Err(AppError::new(
                AppErrorKind::Unavailable,
                "Form limit reached; wait for forms to expire",
            ));
        }
        let nonce = random()?;
        entries.insert(
            (session.clone(), nonce.clone()),
            Entry {
                expires: now.saturating_add(900),
                digest: None,
                completion: None,
            },
        );
        Ok(FormNonce(nonce))
    }
    fn admit(
        &self,
        session: &SessionId,
        nonce: &FormNonce,
        digest: blake3::Hash,
    ) -> AppResult<(watch::Receiver<Option<AppResult<Submission>>>, bool)> {
        let mut entries = self.entries.lock().unwrap();
        let entry = entries
            .get_mut(&(session.clone(), nonce.0.clone()))
            .ok_or_else(|| {
                AppError::new(AppErrorKind::Gone, "Form expired; open a new task form")
            })?;
        if entry.expires <= (self.clock)().as_secs() && !entry.pending() {
            return Err(AppError::new(
                AppErrorKind::Gone,
                "Form expired; open a new task form",
            ));
        }
        if let Some(previous) = entry.digest {
            if previous != digest {
                return Err(AppError::new(
                    AppErrorKind::Conflict,
                    "This form already submitted different inputs; open a new form",
                ));
            }
            return Ok((entry.completion.as_ref().unwrap().subscribe(), false));
        }
        let (tx, rx) = watch::channel(None);
        entry.digest = Some(digest);
        entry.completion = Some(tx);
        Ok((rx, true))
    }
    fn complete(&self, session: &SessionId, nonce: &FormNonce, result: AppResult<Submission>) {
        if let Some(entry) = self
            .entries
            .lock()
            .unwrap()
            .get_mut(&(session.clone(), nonce.0.clone()))
        {
            entry
                .completion
                .as_ref()
                .unwrap()
                .send_replace(Some(result));
        }
    }
    pub async fn submit(
        self: &Arc<Self>,
        manager: &RunnerManager,
        session: &SessionId,
        nonce: &FormNonce,
        request: CreateRequest,
    ) -> AppResult<Submission> {
        let body = serde_json::to_vec(&(
            request.contract_json.as_str(),
            request.worker.as_str(),
            &request.model,
        ))?;
        self.submit_with(manager, session, nonce, blake3::hash(&body), move |home| {
            submission::create(home, request)
        })
        .await
    }
    async fn submit_with(
        self: &Arc<Self>,
        manager: &RunnerManager,
        session: &SessionId,
        nonce: &FormNonce,
        digest: blake3::Hash,
        create: impl FnOnce(&crate::home::Home) -> AppResult<Submission> + Send + 'static,
    ) -> AppResult<Submission> {
        let (mut rx, launch) = self.admit(session, nonce, digest)?;
        if launch {
            let (forms, session, nonce) = (self.clone(), session.clone(), nonce.clone());
            let backup = (session.clone(), nonce.clone());
            let admitted = manager.operation(move |home| {
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| create(home)))
                        .unwrap_or_else(|_| {
                            Err(AppError::new(
                                AppErrorKind::Unavailable,
                                "Submission interrupted; inspect tasks before retrying",
                            ))
                        });
                forms.complete(&session, &nonce, result.clone());
                result
            });
            if let Err(error) = admitted {
                self.complete(&backup.0, &backup.1, Err(error));
            }
        }
        loop {
            let result = rx.borrow().clone();
            if let Some(result) = result {
                return result;
            }
            rx.changed().await.map_err(|_| {
                AppError::new(
                    AppErrorKind::Unavailable,
                    "Submission completion unavailable",
                )
            })?;
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    #[tokio::test]
    async fn form_nonce_owned_submission_publishes_after_lost_response_and_panic() {
        let root = tempfile::tempdir().unwrap();
        let manager = Arc::new(RunnerManager::new(
            crate::home::Home::new(Some(root.path().join("home")), None).unwrap(),
        ));
        let forms = Arc::new(SubmissionForms::new(Arc::new(|| Duration::ZERO)));
        let session = SessionId("session".into());
        let nonce = forms.issue(&session).unwrap();
        let digest = blake3::hash(b"body");
        let (entered, waiting) = tokio::sync::oneshot::channel();
        let release = Arc::new(std::sync::Barrier::new(2));
        let (f, m, s, n, r) = (
            forms.clone(),
            manager.clone(),
            session.clone(),
            nonce.clone(),
            release.clone(),
        );
        let request = tokio::spawn(async move {
            f.submit_with(&m, &s, &n, digest, move |_| {
                entered.send(()).unwrap();
                r.wait();
                Err(AppError::new(AppErrorKind::Invalid, "one durable outcome"))
            })
            .await
        });
        waiting.await.unwrap();
        request.abort();
        release.wait();
        let replay = forms
            .submit_with(&manager, &session, &nonce, digest, |_| {
                panic!("duplicate must not create")
            })
            .await;
        assert_eq!(replay.err().unwrap().message, "one durable outcome");
        let nonce = forms.issue(&session).unwrap();
        let panic = forms
            .submit_with(&manager, &session, &nonce, digest, |_| {
                panic!("submission test panic")
            })
            .await;
        assert_eq!(panic.err().unwrap().kind, AppErrorKind::Unavailable);
        assert!(
            forms
                .submit_with(&manager, &session, &nonce, digest, |_| panic!(
                    "duplicate panic"
                ))
                .await
                .is_err()
        );
        manager.drain().await.unwrap();
    }
    #[test]
    fn form_nonce_coalesces_scopes_bounds_and_preserves_pending_after_expiry() {
        let time = Arc::new(AtomicU64::new(0));
        let clock = time.clone();
        let forms = SubmissionForms::new(Arc::new(move || {
            Duration::from_secs(clock.load(Ordering::SeqCst))
        }));
        let session = SessionId("one".into());
        let nonce = forms.issue(&session).unwrap();
        let digest = blake3::hash(b"body");
        let (rx, first) = forms.admit(&session, &nonce, digest).unwrap();
        assert!(first);
        drop(rx); // Lost response must not lose the completion.
        assert!(!forms.admit(&session, &nonce, digest).unwrap().1);
        assert!(
            forms
                .admit(&session, &nonce, blake3::hash(b"different"))
                .is_err()
        );
        assert!(
            forms
                .admit(&SessionId("two".into()), &nonce, digest)
                .is_err()
        );
        for _ in 1..256 {
            let n = forms.issue(&session).unwrap();
            forms.admit(&session, &n, digest).unwrap();
        }
        time.store(901, Ordering::SeqCst);
        assert!(forms.issue(&session).is_err()); // Pending entries cannot be evicted.
        forms.complete(
            &session,
            &nonce,
            Err(AppError::new(AppErrorKind::Invalid, "test outcome")),
        );
        assert!(forms.admit(&session, &nonce, digest).is_err());
        assert!(forms.issue(&session).is_ok());
        let fresh = forms.issue(&SessionId("three".into())).unwrap();
        time.store(1802, Ordering::SeqCst);
        assert!(
            forms
                .admit(&SessionId("three".into()), &fresh, digest)
                .is_err()
        );
    }
}
