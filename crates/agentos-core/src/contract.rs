use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum ContractError {
    #[error("invalid contract json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid contract: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoRef {
    pub source: String,
    pub revision: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Capability {
    #[serde(rename = "snapshot.read")]
    SnapshotRead,
    #[serde(rename = "workspace.apply_patch")]
    WorkspaceApplyPatch,
    #[serde(rename = "verification.run")]
    VerificationRun,
    #[serde(rename = "artifact.export")]
    ArtifactExport,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub model_requests: u32,
    pub max_output_tokens_per_request: u32,
    pub tool_actions: u32,
    pub deadline_seconds: u32,
    pub worker_vcpus: u32,
    pub worker_memory_mib: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Contract {
    pub goal: String,
    pub repository: RepoRef,
    pub profile: String,
    pub editable_paths: Vec<String>,
    pub verification_profile: String,
    pub capabilities: Vec<Capability>,
    pub limits: Limits,
}

/// A registry id (`profile`, `verification_profile`) must be exactly one plain file name, so
/// joining it onto a registry directory can never leave that directory.
fn check_plain_name(field: &str, v: &str) -> Result<(), ContractError> {
    let plain = !v.is_empty()
        && v != "."
        && v != ".."
        && !v.starts_with('-')
        && !v.contains(['/', '\\', '\0']);
    if plain {
        Ok(())
    } else {
        Err(ContractError::Invalid(format!(
            "{field} {v:?} must be a single plain name (no '/', '\\', '.', '..', NUL or leading '-')"
        )))
    }
}

fn has_parent_component(p: &str) -> bool {
    p.split('/').any(|c| c == "..")
}

impl Contract {
    pub fn parse(json: &str) -> Result<Contract, ContractError> {
        let c: Contract = serde_json::from_str(json)?;
        c.validate()?;
        Ok(c)
    }

    fn validate(&self) -> Result<(), ContractError> {
        let l = &self.limits;
        let limits = [
            ("model_requests", l.model_requests),
            ("max_output_tokens_per_request", l.max_output_tokens_per_request),
            ("tool_actions", l.tool_actions),
            ("deadline_seconds", l.deadline_seconds),
            ("worker_vcpus", l.worker_vcpus),
            ("worker_memory_mib", l.worker_memory_mib),
        ];
        for (name, v) in limits {
            if v == 0 {
                return Err(ContractError::Invalid(format!("limit {name} must be > 0")));
            }
        }
        check_plain_name("profile", &self.profile)?;
        check_plain_name("verification_profile", &self.verification_profile)?;
        if self.editable_paths.is_empty() {
            return Err(ContractError::Invalid("editable_paths must not be empty".into()));
        }
        for p in &self.editable_paths {
            if p.starts_with('/') || has_parent_component(p) {
                return Err(ContractError::Invalid(format!(
                    "editable path {p:?} must be relative and must not contain '..'"
                )));
            }
        }
        Ok(())
    }

    /// True if `rel` (a repo-relative path) matches one of the editable patterns.
    /// `dir/**` matches anything under `dir/`; any other pattern matches exactly.
    pub fn path_allowed(&self, rel: &str) -> bool {
        if rel.starts_with('/') || has_parent_component(rel) {
            return false;
        }
        self.editable_paths.iter().any(|pat| match pat.strip_suffix("**") {
            Some(prefix) if prefix.ends_with('/') => rel.starts_with(prefix),
            _ => rel == pat,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const OK: &str = r#"{"goal":"g","repository":{"source":"/r","revision":"abc"},"profile":"python-stdlib-v1",
      "editable_paths":["src/**"],"verification_profile":"parser-checks-v1",
      "capabilities":["snapshot.read","workspace.apply_patch","verification.run","artifact.export"],
      "limits":{"model_requests":12,"max_output_tokens_per_request":4096,"tool_actions":50,
                "deadline_seconds":1200,"worker_vcpus":2,"worker_memory_mib":2048}}"#;

    #[test] fn parses_spec_example() { assert!(Contract::parse(OK).is_ok()); }
    #[test] fn rejects_unknown_field() {
        let j = OK.replacen("{\"goal\"", "{\"extra\":1,\"goal\"", 1);
        assert!(Contract::parse(&j).is_err());
    }
    #[test] fn rejects_zero_limit() {
        assert!(Contract::parse(&OK.replace("\"tool_actions\":50", "\"tool_actions\":0")).is_err());
    }
    #[test] fn rejects_empty_editable_paths() {
        assert!(Contract::parse(&OK.replace("[\"src/**\"]", "[]")).is_err());
    }
    #[test] fn rejects_escaping_glob() {
        assert!(Contract::parse(&OK.replace("src/**", "../x/**")).is_err());
        assert!(Contract::parse(&OK.replace("src/**", "/etc/**")).is_err());
    }
    #[test] fn rejects_profile_ids_that_are_not_one_plain_name() {
        for bad in ["../../tmp/x", "/abs/path", "a/b", "..", "", ".", "-rf", "a\\b", "a\u{0}b", "x/", "./x"] {
            let id = serde_json::to_string(bad).unwrap();
            let vp = OK.replace("\"verification_profile\":\"parser-checks-v1\"", &format!("\"verification_profile\":{id}"));
            assert_ne!(vp, OK);
            let err = Contract::parse(&vp).unwrap_err().to_string();
            assert!(err.contains("verification_profile"), "{bad:?}: {err}");
            let p = OK.replace("\"profile\":\"python-stdlib-v1\"", &format!("\"profile\":{id}"));
            assert_ne!(p, OK);
            assert!(Contract::parse(&p).unwrap_err().to_string().contains("profile"), "{bad:?}");
        }
        for good in ["parser-checks-v1", "p", "a.b_c-2"] {
            let id = serde_json::to_string(good).unwrap();
            assert!(Contract::parse(&OK.replace("\"parser-checks-v1\"", &id)).is_ok(), "{good:?}");
        }
    }
    #[test] fn path_allowed_matches_glob_and_blocks_traversal() {
        let c = Contract::parse(OK).unwrap();
        assert!(c.path_allowed("src/parser.py"));
        assert!(!c.path_allowed("tests/test_parser.py"));
        assert!(!c.path_allowed("src/../tests/x.py"));
    }
}
