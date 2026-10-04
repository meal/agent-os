use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

pub(crate) const KEY_FILE_LIMIT: usize = 4096;

pub(crate) fn read_key_file(path: &Path) -> io::Result<String> {
    let flags = rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(flags.bits() as i32)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "API key path must be a regular file",
        ));
    }
    if metadata.len() > KEY_FILE_LIMIT as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "API key file exceeds 4096 bytes",
        ));
    }
    let mut bytes = Vec::new();
    file.take((KEY_FILE_LIMIT + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > KEY_FILE_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "API key file exceeds 4096 bytes",
        ));
    }
    String::from_utf8(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "API key file must be UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::process::Command;
    use std::time::{Duration, Instant};

    #[test]
    fn regular_key_files_are_bounded_by_actual_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        fs::write(&path, vec![b'k'; KEY_FILE_LIMIT]).unwrap();
        assert_eq!(read_key_file(&path).unwrap().len(), KEY_FILE_LIMIT);
        fs::write(&path, vec![b'k'; KEY_FILE_LIMIT + 1]).unwrap();
        assert!(read_key_file(&path).is_err());
        fs::write(&path, [0xff]).unwrap();
        assert!(read_key_file(&path).is_err());
    }

    #[test]
    fn nonregular_and_symlink_key_paths_are_refused_promptly() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("key");
        fs::write(&key, b"sk-ant-test-not-a-real-key\n").unwrap();
        let link = dir.path().join("link");
        symlink(&key, &link).unwrap();
        let fifo = dir.path().join("fifo");
        assert!(
            Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        for path in [
            link.as_path(),
            fifo.as_path(),
            dir.path(),
            std::path::Path::new("/dev/null"),
        ] {
            let started = Instant::now();
            assert!(read_key_file(path).is_err(), "{}", path.display());
            assert!(started.elapsed() < Duration::from_secs(1));
        }
        assert!(read_key_file(&key).unwrap().starts_with("sk-ant-test"));
    }
}
