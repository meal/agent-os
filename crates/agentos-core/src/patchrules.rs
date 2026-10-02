//! Patch acceptance rules shared by the host and the guest: they parse the output of
//! `git apply --summary` and `git apply --numstat -z`.

/// Summary lines `git apply --summary` may print for an acceptable patch: plain file
/// creation and deletion. Renames, copies, mode changes and symlinks are refused.
pub const ALLOWED_SUMMARY: [&str; 4] = [
    "create mode 100644 ",
    "create mode 100755 ",
    "delete mode 100644 ",
    "delete mode 100755 ",
];

/// Refuses any `--summary` line that is not a plain create or delete.
pub fn check_summary(summary_stdout: &[u8]) -> Result<(), String> {
    for line in String::from_utf8_lossy(summary_stdout).lines() {
        let line = line.trim_start();
        if !line.is_empty() && !ALLOWED_SUMMARY.iter().any(|p| line.starts_with(p)) {
            return Err(format!("unsupported patch operation: {line}"));
        }
    }
    Ok(())
}

/// Repo-relative paths of a `--numstat -z` listing, in patch order; binary patches and
/// empty listings are refused.
pub fn parse_numstat(numstat_stdout: &[u8]) -> Result<Vec<String>, String> {
    let mut paths = Vec::new();
    for record in numstat_stdout.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let record = std::str::from_utf8(record).map_err(|_| "patch path is not UTF-8".to_string())?;
        let mut fields = record.splitn(3, '\t');
        let (added, deleted, path) = match (fields.next(), fields.next(), fields.next()) {
            (Some(a), Some(d), Some(p)) => (a, d, p),
            _ => return Err(format!("unexpected numstat record {record:?}")),
        };
        if added == "-" || deleted == "-" {
            return Err(format!("binary patches are not supported: {path}"));
        }
        if path.is_empty() {
            return Err(format!("unsupported patch operation in record {record:?}"));
        }
        paths.push(path.to_string());
    }
    if paths.is_empty() {
        return Err("patch touches no files".into());
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_summary_accepts_only_plain_create_and_delete() {
        assert_eq!(check_summary(b" create mode 100644 a\n delete mode 100755 b\n"), Ok(()));
        for bad in [
            " rename src/a => src/b (100%)",
            " mode change 100644 => 100755 x",
            " create mode 120000 l",
        ] {
            let err = check_summary(format!("{bad}\n").as_bytes()).unwrap_err();
            assert_eq!(err, format!("unsupported patch operation: {}", bad.trim_start()));
        }
    }

    #[test]
    fn parse_numstat_reports_paths_in_patch_order_and_refuses_binary() {
        assert_eq!(parse_numstat(b"1\t0\tsrc/a.py\x002\t1\tsrc/b.py\x00").unwrap(), ["src/a.py", "src/b.py"]);
        assert_eq!(
            parse_numstat(b"-\t-\timg.png\x00").unwrap_err(),
            "binary patches are not supported: img.png"
        );
        assert_eq!(parse_numstat(b"").unwrap_err(), "patch touches no files");
        assert_eq!(parse_numstat(b"1\t0\t\xff\x00").unwrap_err(), "patch path is not UTF-8");
    }
}
