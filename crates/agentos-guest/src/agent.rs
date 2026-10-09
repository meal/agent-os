//! The session state machine: `Hello` binds the attempt token and the mode, `Ready`
//! answers, then one request at a time until `Shutdown` or the connection is lost.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use agentos_core::guest::{
    Frame, GUEST_PROTOCOL, Message, Mode, PATCH_LIMIT, is_attempt_token, read_frame, write_frame,
};

use crate::AGENT_VERSION;
use crate::backend::Backend;
use crate::handlers::{self, StreamError};

/// How a session ended; the caller acts on it (fake: exit 0 on `Shutdown`/`Lost`; VM:
/// reboot).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// `Shutdown` was answered with `Bye`.
    Shutdown,
    /// The bound session ended: EOF, an I/O or framing error, a protocol violation, or a
    /// request the mode does not allow.
    Lost,
    /// No session was bound: EOF before `Hello`, a first frame that is not a valid `Hello`,
    /// or a token other than the bound one. The guest keeps listening.
    Rejected,
}

/// The attempt token and the mode bound by the first accepted `Hello`, for the life of the
/// process: a later connection must present both (an inspect `Hello` on a job VM would
/// otherwise remount its workspace read-only mid-job).
static BOUND: OnceLock<(String, Mode)> = OnceLock::new();
/// Set once a `Hello` was accepted; the boot watchdog reads it.
static HELLO_SEEN: AtomicBool = AtomicBool::new(false);
/// Requests are served one at a time, also across connections of the same attempt.
static REQUEST: Mutex<()> = Mutex::new(());

pub struct Session;

fn type_of(m: &Message) -> String {
    serde_json::to_value(m)
        .ok()
        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_string))
        .unwrap_or_else(|| "?".into())
}

fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Job => "job",
        Mode::Inspect => "inspect",
    }
}

fn allowed(mode: Mode, m: &Message) -> bool {
    match mode {
        Mode::Job => matches!(
            m,
            Message::ReadSnapshot { .. }
                | Message::ApplyPatch { .. }
                | Message::RunVerification { .. }
                | Message::RunAgent { .. }
                | Message::Shutdown
        ),
        Mode::Inspect => matches!(
            m,
            Message::Digest | Message::PatchState { .. } | Message::Shutdown
        ),
    }
}

/// Whether a `Hello` has been accepted (bound) in this process.
pub fn hello_seen() -> bool {
    HELLO_SEEN.load(Ordering::SeqCst)
}

/// The boot watchdog: unless a `Hello` has been accepted `after` from now, calls `fire`
/// (the VM shuts down; the fake guest exits). A connection that never completes a valid
/// `Hello` does not count.
pub fn spawn_watchdog(after: Duration, fire: impl FnOnce() + Send + 'static) {
    thread::spawn(move || {
        thread::sleep(after);
        if !hello_seen() {
            eprintln!(
                "agentos-guest: no Hello within {} ms of boot, shutting down",
                after.as_millis()
            );
            fire();
        }
    });
}

fn send(stream: &mut impl Write, m: Message) -> bool {
    write_frame(stream, &Frame::Json(m)).is_ok()
}

fn lost(why: impl std::fmt::Display) -> Exit {
    eprintln!("agentos-guest: session lost: {why}");
    Exit::Lost
}

impl Session {
    /// Serves one connection. `expected` pins the token this connection must present (tests
    /// of one in-process session); `None` (the VM and the fake guest) binds the process-wide
    /// token **and mode** on the first accepted `Hello` and holds every later connection to
    /// both.
    pub fn serve(
        backend: &mut dyn Backend,
        mut stream: impl Read + Write,
        expected: Option<&str>,
    ) -> Exit {
        // `raw_limit` 0 wherever a request is expected: a non-empty raw frame is `TooLarge`
        // before its body is read, an empty one comes back as `Frame::Raw` and is treated as
        // the protocol violation it is.
        let first = match read_frame(&mut stream, 0) {
            Ok(Frame::Json(m)) => m,
            _ => return Exit::Rejected,
        };
        let Message::Hello {
            protocol,
            attempt_token,
            mode,
            ..
        } = first
        else {
            send(
                &mut stream,
                Message::Refused {
                    reason: format!("expected Hello, got {}", type_of(&first)),
                },
            );
            return Exit::Rejected;
        };
        if protocol != GUEST_PROTOCOL {
            let reason = format!(
                "unsupported protocol {protocol}, this agent speaks protocol {GUEST_PROTOCOL}"
            );
            send(&mut stream, Message::Refused { reason });
            return Exit::Rejected;
        }
        // A token that is malformed or not the bound one gets no reply at all.
        if !is_attempt_token(&attempt_token) {
            return Exit::Rejected;
        }
        let accepted = match expected {
            Some(t) => t == attempt_token,
            None => {
                let (token, bound_mode) = BOUND.get_or_init(|| (attempt_token.clone(), mode));
                *token == attempt_token && *bound_mode == mode
            }
        };
        // Another token, or the bound token in another mode: closed without a reply, before
        // anything (the inspect remount included) happens.
        if !accepted {
            return Exit::Rejected;
        }
        HELLO_SEEN.store(true, Ordering::SeqCst);
        if mode == Mode::Inspect
            && let Err(e) = backend.remount_workspace_ro()
        {
            send(
                &mut stream,
                Message::Refused {
                    reason: format!("cannot remount the workspace read-only: {e}"),
                },
            );
            return lost(e);
        }
        let ready = Message::Ready {
            protocol: GUEST_PROTOCOL,
            agent_version: AGENT_VERSION.to_string(),
            mode,
            vcpus: backend.vcpus(),
            memory_mib: backend.memory_mib(),
        };
        if !send(&mut stream, ready) {
            return lost("cannot send Ready");
        }
        loop {
            let request = match read_frame(&mut stream, 0) {
                Ok(Frame::Json(m)) => m,
                Ok(Frame::Raw(_)) => return lost("raw frame where a request was expected"),
                Err(e) => return lost(e),
            };
            if !allowed(mode, &request) {
                let reason = format!(
                    "unexpected request {} in {} mode",
                    type_of(&request),
                    mode_name(mode)
                );
                send(
                    &mut stream,
                    Message::Refused {
                        reason: reason.clone(),
                    },
                );
                return lost(reason);
            }
            let _one_at_a_time = REQUEST.lock().unwrap_or_else(|p| p.into_inner());
            let reply = match request {
                Message::ReadSnapshot {
                    file_count,
                    total_bytes,
                } => match handlers::read_snapshot(backend, &mut stream, file_count, total_bytes) {
                    Ok((files, workspace_digest)) => Message::SnapshotDone {
                        files,
                        workspace_digest,
                    },
                    Err(StreamError::Refused(reason)) => Message::Refused { reason },
                    Err(StreamError::Protocol(why)) => return lost(why),
                },
                Message::ApplyPatch {
                    expected_base,
                    editable_paths,
                } => {
                    let patch = match read_frame(&mut stream, PATCH_LIMIT) {
                        Ok(Frame::Raw(b)) => b,
                        Ok(Frame::Json(_)) => {
                            return lost("JSON frame where the patch was expected");
                        }
                        Err(e) => return lost(e),
                    };
                    match handlers::apply_patch(backend, expected_base, &editable_paths, &patch) {
                        Ok((paths, workspace_digest)) => Message::PatchApplied {
                            paths,
                            workspace_digest,
                        },
                        Err(reason) => Message::Refused { reason },
                    }
                }
                Message::RunVerification {
                    profile_digest,
                    timeout_secs,
                    file_count,
                    total_bytes,
                } => {
                    let profile =
                        match handlers::receive_profile(&mut stream, file_count, total_bytes) {
                            Ok(p) => p,
                            Err(why) => return lost(why),
                        };
                    match handlers::run_verification(backend, profile_digest, timeout_secs, profile)
                    {
                        Ok(v) => v.into_message(),
                        Err(reason) => Message::Refused { reason },
                    }
                }
                Message::Digest => match handlers::digest(backend) {
                    Ok(workspace_digest) => Message::DigestIs { workspace_digest },
                    Err(reason) => Message::Refused { reason },
                },
                Message::PatchState { expected_base } => {
                    let patch = match read_frame(&mut stream, PATCH_LIMIT) {
                        Ok(Frame::Raw(b)) => b,
                        Ok(Frame::Json(_)) => {
                            return lost("JSON frame where the patch was expected");
                        }
                        Err(e) => return lost(e),
                    };
                    if backend.hang_inspect() {
                        // Test hook: never answer, but still end with the connection.
                        loop {
                            if let Err(e) = read_frame(&mut stream, 0) {
                                return lost(e);
                            }
                        }
                    }
                    handlers::patch_state(backend, expected_base, &patch).into_message()
                }
                Message::Shutdown => {
                    if let Err(e) = backend.sync_workspace() {
                        eprintln!("agentos-guest: sync before shutdown failed: {e}");
                    }
                    send(&mut stream, Message::Bye);
                    return Exit::Shutdown;
                }
                Message::RunAgent { .. } => {
                    // Agent sessions land in a later step; until then the request is refused
                    // and the session ends like any unexpected request.
                    let reason = "agent sessions are not implemented yet".to_string();
                    send(
                        &mut stream,
                        Message::Refused {
                            reason: reason.clone(),
                        },
                    );
                    return lost(reason);
                }
                other => return lost(format!("unexpected request {}", type_of(&other))),
            };
            if !send(&mut stream, reply) {
                return lost("cannot send the reply");
            }
        }
    }
}
