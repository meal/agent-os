use super::session::{SessionId, random};
use crate::{
    app::{AppError, AppErrorKind, AppResult, export, queries},
    home::Home,
};
use agentos_core::{broker::Resource, contract::Capability, ids::TaskId};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use std::{
    collections::VecDeque,
    io,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};
#[derive(Clone)]
pub(super) struct DownloadId(pub String);
pub(super) struct ArchiveTicket {
    pub id: DownloadId,
    pub task: TaskId,
    pub bytes: u64,
}
#[derive(serde::Serialize, serde::Deserialize)]
struct Claims {
    nonce: String,
    task: TaskId,
    session: String,
    expires: u64,
}
struct Entry {
    id: String,
    expires: u64,
    owner: Option<Arc<tempfile::TempDir>>,
}
pub(super) struct DownloadCache {
    key: [u8; 32],
    clock: Arc<dyn Fn() -> Duration + Send + Sync>,
    entries: Mutex<VecDeque<Entry>>,
}
pub(super) struct OwnedArchiveReader {
    file: tokio::fs::File,
    pub bytes: u64,
    _owner: Arc<tempfile::TempDir>,
    pub permit: Option<tokio::sync::OwnedSemaphorePermit>,
}
impl tokio::io::AsyncRead for OwnedArchiveReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.file).poll_read(cx, buf)
    }
}
impl DownloadCache {
    pub fn new(clock: Arc<dyn Fn() -> Duration + Send + Sync>) -> AppResult<Self> {
        let mut key = [0; 32];
        getrandom::fill(&mut key)
            .map_err(|_| AppError::new(AppErrorKind::Unavailable, "Random source unavailable"))?;
        Ok(Self {
            key,
            clock,
            entries: Mutex::new(VecDeque::new()),
        })
    }
    fn scope(session: &SessionId) -> String {
        blake3::hash(session.0.as_bytes()).to_hex().to_string()
    }
    fn claims(&self, id: &DownloadId, session: &SessionId) -> AppResult<Claims> {
        let unknown = || AppError::new(AppErrorKind::NotFound, "Download not found");
        if id.0.len() > 768 {
            return Err(unknown());
        }
        let bytes = URL_SAFE_NO_PAD.decode(&id.0).map_err(|_| unknown())?;
        if bytes.len() < 32 {
            return Err(unknown());
        }
        let (body, signature) = bytes.split_at(bytes.len() - 32);
        let sig: [u8; 32] = signature.try_into().map_err(|_| unknown())?;
        if blake3::keyed_hash(&self.key, body) != blake3::Hash::from_bytes(sig) {
            return Err(unknown());
        }
        let claims: Claims = serde_json::from_slice(body).map_err(|_| unknown())?;
        if claims.session != Self::scope(session) {
            return Err(AppError::new(
                AppErrorKind::Forbidden,
                "Download belongs to another local session",
            ));
        }
        if (self.clock)().as_secs() >= claims.expires {
            return Err(AppError::new(
                AppErrorKind::Gone,
                "Download expired; export again",
            ));
        }
        Ok(claims)
    }
    pub fn create(
        &self,
        session: &SessionId,
        home: &Home,
        task: &TaskId,
    ) -> AppResult<ArchiveTicket> {
        let now = (self.clock)().as_secs();
        let claims = Claims {
            nonce: random()?,
            task: task.clone(),
            session: Self::scope(session),
            expires: now.saturating_add(600),
        };
        let mut bytes = serde_json::to_vec(&claims)?;
        let hash = blake3::keyed_hash(&self.key, &bytes);
        bytes.extend(hash.as_bytes());
        let id = DownloadId(URL_SAFE_NO_PAD.encode(bytes));
        {
            let mut entries = self.entries.lock().unwrap();
            entries.retain(|e| e.expires > now || e.owner.is_none());
            if entries.len() >= 8 {
                if let Some(index) = entries.iter().position(|e| e.owner.is_some()) {
                    entries.remove(index);
                } else {
                    return Err(AppError::new(
                        AppErrorKind::Unavailable,
                        "All archive slots are busy",
                    ));
                }
            }
            entries.push_back(Entry {
                id: id.0.clone(),
                expires: claims.expires,
                owner: None,
            });
        }
        let result = (|| {
            let owner = Arc::new(
                tempfile::Builder::new()
                    .prefix("agentos-ui-export-")
                    .tempdir()?,
            );
            let bundle = owner.path().join("bundle");
            export::write(home, task, &bundle, Some(queries::RESULT_LIMIT))?;
            let path = owner.path().join("export.tar");
            let file = std::fs::File::create(&path)?;
            let mut archive = tar::Builder::new(file);
            let mut files = Vec::new();
            regular_files(&bundle, &bundle, &mut files)?;
            files.sort();
            for rel in files {
                let mut file = std::fs::File::open(bundle.join(&rel))?;
                let mut header = tar::Header::new_ustar();
                header.set_size(file.metadata()?.len());
                header.set_mode(0o644);
                header.set_uid(0);
                header.set_gid(0);
                header.set_mtime(0);
                header.set_entry_type(tar::EntryType::Regular);
                header.set_cksum();
                archive.append_data(&mut header, &rel, &mut file)?;
            }
            archive.finish()?;
            let bytes = std::fs::metadata(&path)?.len();
            let mut entries = self.entries.lock().unwrap();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id.0)
                .expect("pending entries cannot be evicted");
            entry.owner = Some(owner);
            Ok(ArchiveTicket {
                id: id.clone(),
                task: task.clone(),
                bytes,
            })
        })();
        if result.is_err() {
            self.entries.lock().unwrap().retain(|e| e.id != id.0);
        }
        result
    }
    pub fn open(
        &self,
        session: &SessionId,
        id: &DownloadId,
        home: &Home,
    ) -> AppResult<OwnedArchiveReader> {
        let claims = self.claims(id, session)?;
        let owner = self
            .entries
            .lock()
            .unwrap()
            .iter()
            .find(|e| e.id == id.0)
            .and_then(|e| e.owner.clone())
            .ok_or_else(|| {
                AppError::new(
                    AppErrorKind::Gone,
                    "Download evicted or unavailable; export again",
                )
            })?;
        let store = home.open()?;
        store
            .db
            .contract_bounded(&claims.task, queries::CONTRACT_LIMIT)?;
        store
            .db
            .check(&claims.task, Capability::ArtifactExport, &Resource::Task)?;
        let file = std::fs::File::open(owner.path().join("export.tar"))?;
        let bytes = file.metadata()?.len();
        Ok(OwnedArchiveReader {
            file: tokio::fs::File::from_std(file),
            bytes,
            _owner: owner,
            permit: None,
        })
    }
}
fn regular_files(
    root: &std::path::Path,
    dir: &std::path::Path,
    files: &mut Vec<std::path::PathBuf>,
) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_dir() {
            regular_files(root, &entry.path(), files)?
        } else if ty.is_file() {
            files.push(entry.path().strip_prefix(root).unwrap().to_owned())
        } else {
            return Err(io::Error::other("Unexpected non-regular bundle entry"));
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use agentos_core::{contract::Contract, ids::Digest, state::TaskEvent};
    #[tokio::test]
    async fn download_cache_creates_an_audited_terminal_archive() {
        let root = tempfile::tempdir().unwrap();
        let home = Home::new(Some(root.path().join("home")), None).unwrap();
        let store = home.open().unwrap();
        let c=Contract::parse(r#"{"goal":"cancelled","repository":{"source":"/repo","revision":"base"},"profile":"python-stdlib-v1","verification_profile":"checks","editable_paths":["src/**"],"capabilities":["artifact.export"],"limits":{"model_requests":1,"max_output_tokens_per_request":100,"tool_actions":10,"deadline_seconds":600,"worker_vcpus":1,"worker_memory_mib":256}}"#).unwrap();
        let id = store
            .db
            .create_task(&c, &Digest::of(&serde_json::to_vec(&c).unwrap()))
            .unwrap();
        store.db.approve_task(&id).unwrap();
        store.db.append(&id, &TaskEvent::CancelRequested).unwrap();
        store.db.append(&id, &TaskEvent::CancelCompleted).unwrap();
        let cache = DownloadCache::new(Arc::new(|| Duration::ZERO)).unwrap();
        let ticket = cache
            .create(&SessionId("session".into()), &home, &id)
            .unwrap();
        assert!(ticket.bytes > 0);
        use tokio::io::AsyncReadExt;
        let session = SessionId("session".into());
        let other = SessionId("other".into());
        let before = store.db.events(&id).unwrap();
        let mut a = cache.open(&session, &ticket.id, &home).unwrap();
        let mut b = cache.open(&session, &ticket.id, &home).unwrap();
        let mut first = vec![0; 100];
        a.read_exact(&mut first).await.unwrap();
        let mut second = vec![0; 100];
        b.read_exact(&mut second).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(store.db.events(&id).unwrap(), before);
        assert_eq!(
            cache.open(&other, &ticket.id, &home).err().unwrap().kind,
            AppErrorKind::Forbidden
        );
        assert_eq!(
            cache
                .open(&session, &DownloadId("tampered".into()), &home)
                .err()
                .unwrap()
                .kind,
            AppErrorKind::NotFound
        );
        let sentinel = root.path().join("outside-sentinel");
        std::fs::write(&sentinel, b"preserve").unwrap();
        for _ in 0..8 {
            cache.create(&session, &home, &id).unwrap();
        }
        assert_eq!(
            cache.open(&session, &ticket.id, &home).err().unwrap().kind,
            AppErrorKind::Gone
        );
        let mut remaining = Vec::new();
        a.read_to_end(&mut remaining).await.unwrap();
        assert_eq!(remaining.len() + 100, ticket.bytes as usize);
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"preserve");
        let clock = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let read_clock = clock.clone();
        let expiring = DownloadCache::new(Arc::new(move || {
            Duration::from_secs(read_clock.load(std::sync::atomic::Ordering::SeqCst))
        }))
        .unwrap();
        let expires = expiring.create(&session, &home, &id).unwrap();
        clock.store(601, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            expiring
                .open(&session, &expires.id, &home)
                .err()
                .unwrap()
                .kind,
            AppErrorKind::Gone
        );
        let current = cache.create(&session, &home, &id).unwrap();
        store
            .db
            .revoke(&id, Some(Capability::ArtifactExport))
            .unwrap();
        let before = store.db.events(&id).unwrap();
        assert_eq!(
            cache.open(&session, &current.id, &home).err().unwrap().kind,
            AppErrorKind::Forbidden
        );
        assert_eq!(store.db.events(&id).unwrap(), before);
    }
}
