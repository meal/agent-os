//! The reference analyzer: file and byte counts, counts by extension, and line counts of
//! UTF-8 text files of the task's snapshot, as one JSON object.

use std::collections::BTreeMap;

wit_bindgen::generate!({ world: "analyzer", path: "../../wit" });

use agentos::analyzer::snapshot::ReadError;

/// Bytes asked for per read; the host allows at most 1 MiB.
const CHUNK: u32 = 1 << 20;

struct Analyzer;

fn error_text(e: ReadError) -> String {
    match e {
        ReadError::Denied(why) => format!("denied: {why}"),
        ReadError::NotFound => "not found".into(),
        ReadError::InvalidPath => "invalid path".into(),
        ReadError::TooLarge => "read too large".into(),
        ReadError::BudgetExhausted => "read budget exhausted".into(),
    }
}

/// `s` as a JSON string literal.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn extension(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => ext,
        _ => "",
    }
}

impl Guest for Analyzer {
    fn analyze(tree: &Tree) -> Result<String, String> {
        let entries = tree.files().map_err(error_text)?;
        let (mut bytes, mut text_files, mut lines) = (0u64, 0u64, 0u64);
        let mut by_extension: BTreeMap<String, u64> = BTreeMap::new();
        for entry in &entries {
            bytes += entry.size;
            *by_extension.entry(extension(&entry.path).to_string()).or_default() += 1;
            let mut content = Vec::with_capacity(entry.size.min(1 << 20) as usize);
            let mut offset = 0u64;
            while offset < entry.size {
                let chunk = tree.read(&entry.path, offset, CHUNK).map_err(error_text)?;
                if chunk.is_empty() {
                    break;
                }
                offset += chunk.len() as u64;
                content.extend_from_slice(&chunk);
            }
            if let Ok(text) = std::str::from_utf8(&content) {
                text_files += 1;
                lines += text.lines().count() as u64;
            }
        }
        let extensions = by_extension
            .iter()
            .map(|(ext, n)| format!("{}:{n}", json_string(ext)))
            .collect::<Vec<_>>()
            .join(",");
        Ok(format!(
            "{{\"analyzer\":\"repo-analyzer-v1\",\"files\":{},\"bytes\":{bytes},\"text_files\":{text_files},\"lines\":{lines},\"extensions\":{{{extensions}}}}}",
            entries.len()
        ))
    }
}

export!(Analyzer);
