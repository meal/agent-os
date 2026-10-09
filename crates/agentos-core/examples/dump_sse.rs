//! Reads one Messages response (JSON) on stdin and writes its SSE replay on stdout; used to
//! feed a real agent CLI canned answers built by the codec the guest proxy uses.
use std::io::{Read, Write};

fn main() {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).expect("stdin");
    let response: serde_json::Value = serde_json::from_str(&input).expect("json");
    std::io::stdout()
        .write_all(&agentos_core::messages::sse_from_message(&response))
        .expect("stdout");
}
