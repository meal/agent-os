use super::{
    AppError, AppErrorKind, AppResult,
    control::{self, ControlOutcome, PreparedRun, RunRequest},
};
use crate::home::Home;
use agentos_core::ids::TaskId;
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Mutex},
    thread::JoinHandle,
};
use tokio::sync::{Semaphore, oneshot};
pub(crate) enum RunAdmission {
    Started(TaskId),
    Reported(ControlOutcome),
}
#[derive(Default)]
struct Threads {
    closed: bool,
    busy: bool,
    task: Option<TaskId>,
    handles: Vec<JoinHandle<()>>,
    error: Option<String>,
}
pub(crate) struct RunnerManager {
    home: Home,
    state: Arc<Mutex<Threads>>,
    controls: Arc<Semaphore>,
}
struct DriverExit(Arc<Mutex<Threads>>);
impl Drop for DriverExit {
    fn drop(&mut self) {
        let mut state = self.0.lock().unwrap();
        state.busy = false;
        state.task = None;
    }
}
fn unavailable(message: &str) -> AppError {
    AppError::new(AppErrorKind::Unavailable, message)
}
fn reap(state: &mut Threads) {
    let mut i = 0;
    while i < state.handles.len() {
        if state.handles[i].is_finished() {
            let _ = state.handles.swap_remove(i).join();
        } else {
            i += 1;
        }
    }
}
pub(crate) fn runtime() -> AppResult<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(AppError::from)
}
impl RunnerManager {
    pub fn new(home: Home) -> Self {
        Self {
            home,
            state: Arc::new(Mutex::new(Threads::default())),
            controls: Arc::new(Semaphore::new(4)),
        }
    }
    pub fn busy(&self) -> bool {
        self.state.lock().unwrap().busy
    }
    pub fn diagnostic(&self) -> Option<String> {
        self.state.lock().unwrap().error.clone()
    }
    pub fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.controls.close();
    }
    pub async fn admit(&self, request: RunRequest) -> AppResult<RunAdmission> {
        self.admission(request)?
            .await
            .map_err(|_| unavailable("Driver admission was interrupted"))?
    }
    fn admission(
        &self,
        request: RunRequest,
    ) -> AppResult<oneshot::Receiver<AppResult<RunAdmission>>> {
        self.admission_with(request, || {})
    }
    fn admission_with(
        &self,
        request: RunRequest,
        after_prepare: impl FnOnce() + Send + 'static,
    ) -> AppResult<oneshot::Receiver<AppResult<RunAdmission>>> {
        let (tx, rx) = oneshot::channel();
        let mut state = self.state.lock().unwrap();
        reap(&mut state);
        if state.closed {
            return Err(unavailable("The UI is shutting down"));
        }
        if state.busy {
            return Err(AppError::new(
                AppErrorKind::Conflict,
                "A task is already being driven in this home",
            ));
        }
        state.busy = true;
        state.task = Some(request.task.clone());
        state.error = None;
        let home = self.home.clone();
        let shared = self.state.clone();
        let spawn = std::thread::Builder::new()
            .name("agentos-ui-driver".into())
            .spawn(move || {
                let _exit = DriverExit(shared.clone());
                let mut tx = Some(tx);
                let result = catch_unwind(AssertUnwindSafe(|| -> AppResult<()> {
                    let runtime = runtime()?;
                    let task = request.task.clone();
                    let prepared = control::prepare(&home, request)?;
                    after_prepare();
                    let admission = match &prepared {
                        PreparedRun::Report(r) => RunAdmission::Reported(r.clone()),
                        _ => RunAdmission::Started(task),
                    };
                    let _ = tx.take().unwrap().send(Ok(admission));
                    runtime.block_on(control::drive_prepared(prepared))?;
                    Ok(())
                }))
                .unwrap_or_else(|_| {
                    Err(unavailable(
                        "The driver exited unexpectedly; recover using the recorded state",
                    ))
                });
                if let Err(error) = result {
                    if let Some(tx) = tx {
                        let _ = tx.send(Err(error.clone()));
                    }
                    shared.lock().unwrap().error = Some(error.message);
                }
            });
        match spawn {
            Ok(handle) => state.handles.push(handle),
            Err(e) => {
                state.busy = false;
                state.task = None;
                return Err(e.into());
            }
        }
        Ok(rx)
    }
    pub fn operation<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&Home) -> AppResult<T> + Send + 'static,
    ) -> AppResult<oneshot::Receiver<AppResult<T>>> {
        let mut state = self.state.lock().unwrap();
        reap(&mut state);
        if state.closed {
            return Err(unavailable("The UI is shutting down"));
        }
        let permit = self
            .controls
            .clone()
            .try_acquire_owned()
            .map_err(|_| unavailable("Control service is busy; try again"))?;
        let (tx, rx) = oneshot::channel();
        let home = self.home.clone();
        let shared = self.state.clone();
        let handle = std::thread::Builder::new()
            .name("agentos-ui-control".into())
            .spawn(move || {
                let _permit = permit;
                let result = catch_unwind(AssertUnwindSafe(|| operation(&home)))
                    .unwrap_or_else(|_| Err(unavailable("Control operation exited unexpectedly")));
                if let Err(error) = &result {
                    shared.lock().unwrap().error = Some(error.message.clone());
                }
                let _ = tx.send(result);
            })?;
        state.handles.push(handle);
        Ok(rx)
    }
    pub async fn drain(&self) -> AppResult<()> {
        self.close();
        let handles = std::mem::take(&mut self.state.lock().unwrap().handles);
        tokio::task::spawn_blocking(move || {
            for handle in handles {
                let _ = handle.join();
            }
        })
        .await
        .map_err(|_| unavailable("Could not await owned operations"))?;
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicBool, Ordering},
    };
    #[tokio::test]
    async fn runner_owned_operations_survive_disconnect_and_shutdown_rejects_admission() {
        let root = tempfile::tempdir().unwrap();
        let home = Home::new(Some(root.path().join("home")), None).unwrap();
        let manager = RunnerManager::new(home);
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let finished = Arc::new(AtomicBool::new(false));
        let (e, r, f) = (entered.clone(), release.clone(), finished.clone());
        let response = manager
            .operation(move |_| {
                e.wait();
                r.wait();
                f.store(true, Ordering::SeqCst);
                Ok(())
            })
            .unwrap();
        entered.wait();
        drop(response);
        release.wait();
        manager.drain().await.unwrap();
        assert!(finished.load(Ordering::SeqCst));
        assert!(manager.operation(|_| Ok(())).is_err());
    }
    #[tokio::test]
    async fn runner_bounds_auxiliary_operations_and_releases_after_panic() {
        let root = tempfile::tempdir().unwrap();
        let manager = RunnerManager::new(Home::new(Some(root.path().join("home")), None).unwrap());
        let release = Arc::new(Barrier::new(5));
        let mut responses = Vec::new();
        for _ in 0..4 {
            let r = release.clone();
            responses.push(
                manager
                    .operation(move |_| {
                        r.wait();
                        Ok(())
                    })
                    .unwrap(),
            );
        }
        assert!(manager.operation(|_| Ok(())).is_err());
        release.wait();
        for response in responses {
            response.await.unwrap().unwrap();
        }
        let panic = manager
            .operation::<()>(|_| panic!("owned test panic"))
            .unwrap();
        assert_eq!(
            panic.await.unwrap().unwrap_err().kind,
            AppErrorKind::Unavailable
        );
        manager
            .operation(|_| Ok(()))
            .unwrap()
            .await
            .unwrap()
            .unwrap();
        manager.drain().await.unwrap();
    }
    #[tokio::test]
    async fn runner_admission_survives_disconnect_and_panic_releases_gate() {
        let (_root, home, task) = super::super::control::tests::fixture();
        control::cancel(&home, &task).await.unwrap();
        let manager = RunnerManager::new(home);
        let request = || RunRequest {
            task: task.clone(),
            patch: None,
            crash: None,
            reviewed_contract: None,
        };
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let (e, r) = (entered.clone(), release.clone());
        let response = manager
            .admission_with(request(), move || {
                e.wait();
                r.wait();
            })
            .unwrap();
        entered.wait();
        drop(response);
        assert!(manager.admission(request()).is_err());
        release.wait();
        while manager.busy() {
            tokio::task::yield_now().await;
        }
        let panic = manager
            .admission_with(request(), || panic!("driver test panic"))
            .unwrap();
        assert!(panic.await.unwrap().is_err());
        while manager.busy() {
            tokio::task::yield_now().await;
        }
        assert!(matches!(
            manager.admit(request()).await.unwrap(),
            RunAdmission::Reported(_)
        ));
        manager.drain().await.unwrap();
    }
}
