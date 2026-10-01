use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use agentos_core::ids::Digest;
use uuid::Uuid;

/// Content-addressed blob store: `<dir>/objects/<2 hex>/<62 hex>` plus `<dir>/tmp/`.
pub struct BlobStore {
    objects: PathBuf,
    tmp: PathBuf,
}

fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

fn is_lower_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

impl BlobStore {
    pub fn open(dir: impl AsRef<Path>) -> io::Result<BlobStore> {
        let dir = dir.as_ref();
        let objects = dir.join("objects");
        let tmp = dir.join("tmp");
        fs::create_dir_all(&objects)?;
        fs::create_dir_all(&tmp)?;
        Ok(BlobStore { objects, tmp })
    }

    fn object_path(&self, d: &Digest) -> PathBuf {
        let hex = d.to_string();
        self.objects.join(&hex[..2]).join(&hex[2..])
    }

    pub fn put(&self, bytes: &[u8]) -> io::Result<Digest> {
        let digest = Digest::of(bytes);
        let dest = self.object_path(&digest);
        let shard = dest.parent().expect("object path has a shard directory");

        let tmp_path = self.tmp.join(Uuid::new_v4().to_string());
        let mut file = File::create_new(&tmp_path)?;
        let written = file.write_all(bytes).and_then(|()| file.sync_all());
        drop(file);
        if let Err(e) = written {
            let _ = fs::remove_file(&tmp_path);
            return Err(e);
        }

        let placed = fs::create_dir_all(shard).and_then(|()| {
            if dest.is_file() {
                fs::remove_file(&tmp_path)
            } else {
                fs::rename(&tmp_path, &dest)
            }
        });
        if let Err(e) = placed {
            let _ = fs::remove_file(&tmp_path);
            return Err(e);
        }

        sync_dir(shard)?;
        sync_dir(&self.objects)?;
        Ok(digest)
    }

    pub fn get(&self, d: &Digest) -> io::Result<Vec<u8>> {
        let bytes = fs::read(self.object_path(d))?;
        if Digest::of(&bytes) != *d {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("blob {d} failed integrity check"),
            ));
        }
        Ok(bytes)
    }

    pub fn exists(&self, d: &Digest) -> bool {
        self.object_path(d).is_file()
    }

    /// Removes every object not in `referenced` and every leftover temp file.
    /// Returns the number of files removed. Names that are not valid object
    /// names are left untouched.
    pub fn gc(&self, referenced: &HashSet<Digest>) -> io::Result<usize> {
        let mut removed = 0;
        for shard in fs::read_dir(&self.objects)? {
            let shard = shard?;
            let shard_name = shard.file_name();
            let Some(shard_name) = shard_name.to_str() else { continue };
            if !is_lower_hex(shard_name, 2) || !shard.file_type()?.is_dir() {
                continue;
            }
            for entry in fs::read_dir(shard.path())? {
                let entry = entry?;
                let name = entry.file_name();
                let Some(name) = name.to_str() else { continue };
                if !is_lower_hex(name, 62) || !entry.file_type()?.is_file() {
                    continue;
                }
                let Ok(digest) = Digest::from_hex(&format!("{shard_name}{name}")) else {
                    continue;
                };
                if !referenced.contains(&digest) {
                    fs::remove_file(entry.path())?;
                    removed += 1;
                }
            }
        }
        for entry in fs::read_dir(&self.tmp)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                fs::remove_file(entry.path())?;
                removed += 1;
            }
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Arc;

    fn store() -> (tempfile::TempDir, BlobStore) {
        let dir = tempfile::tempdir().unwrap();
        let s = BlobStore::open(dir.path()).unwrap();
        (dir, s)
    }

    fn object_path(dir: &Path, d: &Digest) -> PathBuf {
        let hex = d.to_string();
        dir.join("objects").join(&hex[..2]).join(&hex[2..])
    }

    fn count_objects(dir: &Path) -> usize {
        let mut n = 0;
        for sub in fs::read_dir(dir.join("objects")).unwrap() {
            n += fs::read_dir(sub.unwrap().path()).unwrap().count();
        }
        n
    }

    #[test]
    fn open_creates_layout_and_is_reopenable() {
        let dir = tempfile::tempdir().unwrap();
        let s = BlobStore::open(dir.path()).unwrap();
        assert!(dir.path().join("objects").is_dir());
        assert!(dir.path().join("tmp").is_dir());
        let d = s.put(b"keep").unwrap();
        let s2 = BlobStore::open(dir.path()).unwrap();
        assert_eq!(s2.get(&d).unwrap(), b"keep");
    }

    #[test]
    fn round_trip() {
        let (_t, s) = store();
        let d = s.put(b"hello world").unwrap();
        assert_eq!(d, Digest::of(b"hello world"));
        assert!(s.exists(&d));
        assert_eq!(s.get(&d).unwrap(), b"hello world");
    }

    #[test]
    fn put_twice_same_digest_one_file() {
        let (t, s) = store();
        let a = s.put(b"same").unwrap();
        let b = s.put(b"same").unwrap();
        assert_eq!(a, b);
        assert_eq!(count_objects(t.path()), 1);
        assert_eq!(fs::read_dir(t.path().join("tmp")).unwrap().count(), 0);
    }

    #[test]
    fn put_empty_bytes() {
        let (_t, s) = store();
        let d = s.put(b"").unwrap();
        assert_eq!(d, Digest::of(b""));
        assert!(s.exists(&d));
        assert!(s.get(&d).unwrap().is_empty());
    }

    #[test]
    fn get_missing_is_not_found() {
        let (_t, s) = store();
        let err = s.get(&Digest::of(b"absent")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(!s.exists(&Digest::of(b"absent")));
    }

    #[test]
    fn corrupted_blob_fails_integrity_check() {
        let (t, s) = store();
        let d = s.put(b"original").unwrap();
        fs::write(object_path(t.path(), &d), b"tampered").unwrap();
        let err = s.get(&d).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn leftover_tmp_file_is_invisible_and_gc_removes_it() {
        let (t, s) = store();
        let crashed = b"half written";
        let leftover = t.path().join("tmp").join("crashed-put");
        fs::write(&leftover, crashed).unwrap();
        let d = Digest::of(crashed);
        assert!(!s.exists(&d));
        assert_eq!(s.get(&d).unwrap_err().kind(), io::ErrorKind::NotFound);
        let removed = s.gc(&HashSet::new()).unwrap();
        assert_eq!(removed, 1);
        assert!(!leftover.exists());
    }

    #[test]
    fn gc_removes_unreferenced_keeps_referenced() {
        let (t, s) = store();
        let keep = s.put(b"keep").unwrap();
        let drop1 = s.put(b"drop1").unwrap();
        let drop2 = s.put(b"drop2").unwrap();
        let referenced: HashSet<Digest> = [keep].into_iter().collect();
        assert_eq!(s.gc(&referenced).unwrap(), 2);
        assert!(s.exists(&keep));
        assert!(!s.exists(&drop1));
        assert!(!s.exists(&drop2));
        assert_eq!(s.get(&keep).unwrap(), b"keep");
        assert_eq!(s.gc(&referenced).unwrap(), 0);
        assert_eq!(count_objects(t.path()), 1);
    }

    #[test]
    fn gc_counts_objects_and_tmp_files_and_ignores_junk() {
        let (t, s) = store();
        s.put(b"gone").unwrap();
        fs::write(t.path().join("tmp").join("leftover"), b"x").unwrap();
        let junk_dir = t.path().join("objects").join("zz");
        fs::create_dir(&junk_dir).unwrap();
        fs::write(junk_dir.join("not-hex"), b"junk").unwrap();
        fs::write(t.path().join("objects").join("stray-file"), b"junk").unwrap();
        assert_eq!(s.gc(&HashSet::new()).unwrap(), 2);
        assert!(junk_dir.join("not-hex").exists());
        assert!(t.path().join("objects").join("stray-file").exists());
    }

    #[test]
    fn concurrent_identical_puts_all_succeed() {
        let (t, s) = store();
        let s = Arc::new(s);
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let s = Arc::clone(&s);
                std::thread::spawn(move || s.put(b"contended payload").unwrap())
            })
            .collect();
        let digests: Vec<Digest> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert!(digests.iter().all(|d| *d == Digest::of(b"contended payload")));
        assert_eq!(count_objects(t.path()), 1);
        assert_eq!(fs::read_dir(t.path().join("tmp")).unwrap().count(), 0);
        assert_eq!(s.get(&digests[0]).unwrap(), b"contended payload");
    }
}
