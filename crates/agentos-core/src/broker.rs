use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::contract::{Capability, Contract, path_matches};
use crate::ids::TaskId;

#[derive(Debug, thiserror::Error)]
pub enum BrokerError {
    #[error("invalid handle: {0}")]
    InvalidHandle(String),
}

/// Unguessable capability handle: 16 random bytes as 32 lowercase hex characters.
/// `Debug` prints only the first 8 characters so logs never leak a usable handle.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Handle(String);

impl Handle {
    pub fn generate() -> Handle {
        let mut buf = [0u8; 16];
        getrandom::fill(&mut buf).expect("OS randomness unavailable");
        Handle(buf.iter().map(|b| format!("{b:02x}")).collect())
    }

    pub fn parse(s: &str) -> Result<Handle, BrokerError> {
        let ok = s.len() == 32 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if ok {
            Ok(Handle(s.to_string()))
        } else {
            Err(BrokerError::InvalidHandle(
                "must be exactly 32 lowercase hex characters".into(),
            ))
        }
    }

    pub fn prefix(&self) -> &str {
        &self.0[..8]
    }
}

impl fmt::Display for Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Handle({}…)", self.prefix())
    }
}

impl Serialize for Handle {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Handle {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Handle, D::Error> {
        let s = String::deserialize(d)?;
        Handle::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// What a grant covers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Scope {
    Paths(Vec<String>),
    Profile(String),
    Task,
}

/// What a caller asks to touch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Resource {
    Paths(Vec<String>),
    Profile(String),
    Task,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityGrant {
    pub handle: Handle,
    pub task: TaskId,
    pub operation: Capability,
    pub scope: Scope,
    /// Unix seconds; `None` means no expiry.
    pub expires_ts: Option<i64>,
    pub revoked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denial {
    UnknownHandle,
    WrongTask,
    WrongOperation,
    OutOfScope,
    Expired,
    Revoked,
}

impl Denial {
    pub fn reason(&self) -> &'static str {
        match self {
            Denial::UnknownHandle => "unknown_handle",
            Denial::WrongTask => "wrong_task",
            Denial::WrongOperation => "wrong_operation",
            Denial::OutOfScope => "out_of_scope",
            Denial::Expired => "expired",
            Denial::Revoked => "revoked",
        }
    }
}

/// Pure authorization rule. Checks, in order: task, operation, revoked, expired, scope.
pub fn authorize(
    grant: &CapabilityGrant,
    task: &TaskId,
    op: Capability,
    resource: &Resource,
    now: i64,
) -> Result<(), Denial> {
    if &grant.task != task {
        return Err(Denial::WrongTask);
    }
    if grant.operation != op {
        return Err(Denial::WrongOperation);
    }
    if grant.revoked {
        return Err(Denial::Revoked);
    }
    if grant.expires_ts.is_some_and(|e| now >= e) {
        return Err(Denial::Expired);
    }
    let in_scope = match (&grant.scope, resource) {
        (Scope::Paths(pats), Resource::Paths(paths)) => {
            !paths.is_empty() && paths.iter().all(|p| path_matches(pats, p))
        }
        (Scope::Profile(a), Resource::Profile(b)) => a == b,
        (Scope::Task, Resource::Task) => true,
        _ => false,
    };
    if in_scope {
        Ok(())
    } else {
        Err(Denial::OutOfScope)
    }
}

/// The scope a contract implies for an operation.
pub fn scope_for(op: Capability, contract: &Contract) -> Scope {
    match op {
        Capability::WorkspaceApplyPatch => Scope::Paths(contract.editable_paths.clone()),
        Capability::SnapshotRead | Capability::ArtifactExport | Capability::ModelRequest => {
            Scope::Task
        }
        Capability::VerificationRun => Scope::Profile(contract.verification_profile.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{Capability, Contract};
    use crate::ids::TaskId;

    fn grant(task: &TaskId, scope: Scope, expires: Option<i64>, revoked: bool) -> CapabilityGrant {
        CapabilityGrant {
            handle: Handle::generate(),
            task: task.clone(),
            operation: Capability::WorkspaceApplyPatch,
            scope,
            expires_ts: expires,
            revoked,
        }
    }
    fn paths(p: &[&str]) -> Resource {
        Resource::Paths(p.iter().map(|s| s.to_string()).collect())
    }
    fn src_scope() -> Scope {
        Scope::Paths(vec!["src/**".into()])
    }

    #[test]
    fn handles_are_32_lowercase_hex_and_unique() {
        let (a, b) = (Handle::generate(), Handle::generate());
        assert_ne!(a, b);
        for h in [&a, &b] {
            let s = h.to_string();
            assert_eq!(s.len(), 32);
            assert!(s.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')));
            assert_eq!(h.prefix(), &s[..8]);
            assert_eq!(Handle::parse(&s).unwrap(), *h);
        }
    }

    #[test]
    fn debug_never_prints_the_full_handle() {
        let h = Handle::generate();
        let dbg = format!("{h:?}");
        assert!(dbg.contains(h.prefix()));
        assert!(dbg.contains('…'));
        assert!(!dbg.contains(&h.to_string()));
        let g = grant(&TaskId::new(), src_scope(), None, false);
        assert!(!format!("{g:?}").contains(&g.handle.to_string()));
    }

    #[test]
    fn handle_serde_is_hex_string_and_validated() {
        let h = Handle::generate();
        let json = serde_json::to_string(&h).unwrap();
        assert_eq!(json, format!("\"{h}\""));
        assert_eq!(serde_json::from_str::<Handle>(&json).unwrap(), h);
        assert!(serde_json::from_str::<Handle>("\"abc\"").is_err());
    }

    #[test]
    fn parse_rejects_empty_31_33_chars_uppercase_and_non_hex() {
        let ok = "0123456789abcdef0123456789abcdef";
        assert!(Handle::parse(ok).is_ok());
        assert!(Handle::parse("").is_err());
        assert!(Handle::parse(&ok[..31]).is_err());
        assert!(Handle::parse(&format!("{ok}0")).is_err());
        assert!(Handle::parse(&ok.to_uppercase()).is_err());
        assert!(Handle::parse(&format!("{}g", &ok[..31])).is_err());
        assert!(matches!(
            Handle::parse("x"),
            Err(BrokerError::InvalidHandle(_))
        ));
    }

    #[test]
    fn authorize_checks_each_denial_in_order() {
        let t = TaskId::new();
        let op = Capability::WorkspaceApplyPatch;
        let g = grant(&t, src_scope(), Some(100), false);
        let r = paths(&["src/a.py"]);
        assert_eq!(
            authorize(&g, &TaskId::new(), op, &r, 0),
            Err(Denial::WrongTask)
        );
        assert_eq!(
            authorize(&g, &t, Capability::SnapshotRead, &r, 0),
            Err(Denial::WrongOperation)
        );
        let revoked = grant(&t, src_scope(), Some(100), true);
        assert_eq!(authorize(&revoked, &t, op, &r, 1000), Err(Denial::Revoked));
        assert_eq!(authorize(&g, &t, op, &r, 100), Err(Denial::Expired));
        assert_eq!(
            authorize(&g, &t, op, &paths(&["tests/x.py"]), 99),
            Err(Denial::OutOfScope)
        );
        assert_eq!(
            authorize(&g, &t, op, &paths(&["src/../tests/x"]), 99),
            Err(Denial::OutOfScope)
        );
        assert_eq!(
            authorize(&g, &t, op, &paths(&["src/a.py", "tests/x.py"]), 99),
            Err(Denial::OutOfScope)
        );
        assert_eq!(authorize(&g, &t, op, &r, 99), Ok(()));
    }

    #[test]
    fn adjacent_denials_keep_their_order() {
        let t = TaskId::new();
        let op = Capability::WorkspaceApplyPatch;
        let bad = paths(&["tests/x.py"]);
        let g = grant(&t, src_scope(), Some(100), false);
        assert_eq!(
            authorize(&g, &TaskId::new(), Capability::SnapshotRead, &bad, 0),
            Err(Denial::WrongTask)
        );
        let revoked = grant(&t, src_scope(), Some(100), true);
        assert_eq!(authorize(&revoked, &t, op, &bad, 0), Err(Denial::Revoked));
        assert_eq!(authorize(&g, &t, op, &bad, 100), Err(Denial::Expired));
    }

    #[test]
    fn empty_paths_resource_or_scope_is_denied() {
        let t = TaskId::new();
        let op = Capability::WorkspaceApplyPatch;
        let g = grant(&t, src_scope(), None, false);
        assert_eq!(
            authorize(&g, &t, op, &Resource::Paths(vec![]), 0),
            Err(Denial::OutOfScope)
        );
        let none = grant(&t, Scope::Paths(vec![]), None, false);
        assert_eq!(
            authorize(&none, &t, op, &paths(&["src/a.py"]), 0),
            Err(Denial::OutOfScope)
        );
        assert_eq!(
            authorize(&none, &t, op, &Resource::Paths(vec![]), 0),
            Err(Denial::OutOfScope)
        );
    }

    #[test]
    fn no_expiry_never_expires() {
        let t = TaskId::new();
        let g = grant(&t, src_scope(), None, false);
        assert_eq!(
            authorize(
                &g,
                &t,
                Capability::WorkspaceApplyPatch,
                &paths(&["src/a"]),
                i64::MAX
            ),
            Ok(())
        );
    }

    #[test]
    fn profile_and_task_scopes_are_exact() {
        let t = TaskId::new();
        let op = Capability::WorkspaceApplyPatch;
        let p = grant(&t, Scope::Profile("p1".into()), None, false);
        assert_eq!(
            authorize(&p, &t, op, &Resource::Profile("p1".into()), 0),
            Ok(())
        );
        assert_eq!(
            authorize(&p, &t, op, &Resource::Profile("p2".into()), 0),
            Err(Denial::OutOfScope)
        );
        assert_eq!(
            authorize(&p, &t, op, &Resource::Task, 0),
            Err(Denial::OutOfScope)
        );
        let k = grant(&t, Scope::Task, None, false);
        assert_eq!(authorize(&k, &t, op, &Resource::Task, 0), Ok(()));
        assert_eq!(
            authorize(&k, &t, op, &paths(&["src/a"]), 0),
            Err(Denial::OutOfScope)
        );
        assert_eq!(
            authorize(&k, &t, op, &Resource::Profile("p1".into()), 0),
            Err(Denial::OutOfScope)
        );
    }

    #[test]
    fn denial_reasons_are_stable_strings() {
        let all = [
            (Denial::UnknownHandle, "unknown_handle"),
            (Denial::WrongTask, "wrong_task"),
            (Denial::WrongOperation, "wrong_operation"),
            (Denial::OutOfScope, "out_of_scope"),
            (Denial::Expired, "expired"),
            (Denial::Revoked, "revoked"),
        ];
        for (d, s) in all {
            assert_eq!(d.reason(), s);
        }
    }

    #[test]
    fn scope_serde_is_tagged_and_round_trips() {
        let s = Scope::Paths(vec!["src/**".into()]);
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, r#"{"kind":"paths","value":["src/**"]}"#);
        assert_eq!(serde_json::from_str::<Scope>(&json).unwrap(), s);
        assert_eq!(
            serde_json::to_string(&Scope::Task).unwrap(),
            r#"{"kind":"task"}"#
        );
    }

    #[test]
    fn scope_for_maps_contract_fields() {
        let c = Contract::parse(
            r#"{"goal":"g","repository":{"source":"/r","revision":"abc"},"profile":"python-stdlib-v1",
      "editable_paths":["src/**"],"verification_profile":"parser-checks-v1",
      "capabilities":["snapshot.read"],
      "limits":{"model_requests":1,"max_output_tokens_per_request":1,"tool_actions":1,
                "deadline_seconds":1,"worker_vcpus":1,"worker_memory_mib":1}}"#,
        )
        .unwrap();
        assert_eq!(
            scope_for(Capability::WorkspaceApplyPatch, &c),
            Scope::Paths(vec!["src/**".into()])
        );
        assert_eq!(scope_for(Capability::SnapshotRead, &c), Scope::Task);
        assert_eq!(scope_for(Capability::ArtifactExport, &c), Scope::Task);
        assert_eq!(scope_for(Capability::ModelRequest, &c), Scope::Task);
        assert_eq!(
            scope_for(Capability::VerificationRun, &c),
            Scope::Profile("parser-checks-v1".into())
        );
    }
}
