//! A test analyzer that misbehaves on request. The first line of the snapshot's
//! `agentos-mode` file says how:
//!
//! - `ok`: a small valid report
//! - `loop`: never returns
//! - `grow`: allocates until something stops it
//! - `oversize`: a valid JSON object over the 64 KiB report limit
//! - `malformed`, `array`: a report that is not a JSON object
//! - `err`: gives up with an error
//! - `claim-pass`: a report that claims a verification pass
//! - `read <path>`: reads `path` once
//! - `read-twice <path>`: reads `path` twice
//! - `big-read <path>`: asks for 2 MiB in one read
//! - `many-reads <path>`: reads one byte 20000 times
//!
//! A read error becomes the component's `Err`, so the host's refusal is visible.

wit_bindgen::generate!({ world: "analyzer", path: "../../wit" });

use agentos::analyzer::snapshot::ReadError;

struct Hostile;

fn error_text(e: ReadError) -> String {
    match e {
        ReadError::Denied(why) => format!("denied: {why}"),
        ReadError::NotFound => "not found".into(),
        ReadError::InvalidPath => "invalid path".into(),
        ReadError::TooLarge => "read too large".into(),
        ReadError::BudgetExhausted => "read budget exhausted".into(),
    }
}

fn mode(tree: &Tree) -> Result<String, String> {
    let bytes = tree.read("agentos-mode", 0, 4096).map_err(error_text)?;
    Ok(String::from_utf8_lossy(&bytes).lines().next().unwrap_or("").to_string())
}

impl Guest for Hostile {
    fn analyze(tree: &Tree) -> Result<String, String> {
        let mode = mode(tree)?;
        let (verb, arg) = mode.split_once(' ').unwrap_or((mode.as_str(), ""));
        match verb {
            "ok" => Ok("{\"ok\":true}".into()),
            "loop" => {
                let mut n = 0u64;
                loop {
                    n = std::hint::black_box(n.wrapping_add(1));
                }
            }
            "grow" => {
                let mut kept: Vec<Vec<u8>> = Vec::new();
                loop {
                    kept.push(vec![1u8; 1 << 20]);
                    std::hint::black_box(&kept);
                }
            }
            "oversize" => Ok(format!("{{\"padding\":\"{}\"}}", "x".repeat(100 * 1024))),
            "malformed" => Ok("this is not json".into()),
            "array" => Ok("[1,2,3]".into()),
            "err" => Err("the analyzer gave up".into()),
            "claim-pass" => Ok("{\"passed\":true,\"verified\":true}".into()),
            "read" => {
                let bytes = tree.read(arg, 0, 1024).map_err(error_text)?;
                Ok(format!("{{\"read\":{}}}", bytes.len()))
            }
            "read-twice" => {
                let first = tree.read(arg, 0, 1024).map_err(error_text)?;
                let second = tree.read(arg, 0, 1024).map_err(error_text)?;
                Ok(format!("{{\"read\":{}}}", first.len() + second.len()))
            }
            "big-read" => {
                let bytes = tree.read(arg, 0, 2 << 20).map_err(error_text)?;
                Ok(format!("{{\"read\":{}}}", bytes.len()))
            }
            "many-reads" => {
                for _ in 0..20_000 {
                    tree.read(arg, 0, 1).map_err(error_text)?;
                }
                Ok("{\"reads\":20000}".into())
            }
            other => Err(format!("unknown mode {other:?}")),
        }
    }
}

export!(Hostile);
