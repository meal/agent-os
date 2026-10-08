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
    #[serde(rename = "model.request")]
    ModelRequest,
}

/// Bounds of the optional VM resource limits (see `resources::VmResources` for their
/// defaults). The workspace drive must hold a 256 MiB snapshot with ext4 overhead; scratch the
/// staged profile (64 MiB) and the reverse-check copy of the workspace content (256 MiB).
pub const WORKER_DISK_MIB: std::ops::RangeInclusive<u32> = 512..=32768;
pub const WORKER_SCRATCH_MIB: std::ops::RangeInclusive<u32> = 384..=32768;
/// The minimum rates keep boot (formatting scratch before the guest's 10 s watchdog), a
/// near-limit snapshot (120 s reply deadline) and its inspection (60 s) within their
/// deadlines; measured, see the VM resources design.
pub const WORKER_DISK_BANDWIDTH_MIB_S: std::ops::RangeInclusive<u32> = 32..=4096;
pub const WORKER_DISK_IOPS: std::ops::RangeInclusive<u32> = 5000..=1_000_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub model_requests: u32,
    pub max_output_tokens_per_request: u32,
    pub tool_actions: u32,
    pub deadline_seconds: u32,
    pub worker_vcpus: u32,
    pub worker_memory_mib: u32,
    // Optional, and omitted when absent, so a contract without them keeps its digest.
    /// The workspace drive's size; absent means the version's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_disk_mib: Option<u32>,
    /// The scratch drive's size; absent means the version's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_scratch_mib: Option<u32>,
    /// Bandwidth of each writable drive; absent means unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_disk_bandwidth_mib_s: Option<u32>,
    /// Operations per second of each writable drive; absent means unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_disk_iops: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Contract {
    pub goal: String,
    pub repository: RepoRef,
    pub profile: String,
    pub editable_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guest_image_digest: Option<String>,
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
            (
                "max_output_tokens_per_request",
                l.max_output_tokens_per_request,
            ),
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
        let bounded = [
            ("worker_disk_mib", l.worker_disk_mib, WORKER_DISK_MIB),
            (
                "worker_scratch_mib",
                l.worker_scratch_mib,
                WORKER_SCRATCH_MIB,
            ),
            (
                "worker_disk_bandwidth_mib_s",
                l.worker_disk_bandwidth_mib_s,
                WORKER_DISK_BANDWIDTH_MIB_S,
            ),
            ("worker_disk_iops", l.worker_disk_iops, WORKER_DISK_IOPS),
        ];
        for (name, v, range) in bounded {
            if let Some(v) = v
                && !range.contains(&v)
            {
                return Err(ContractError::Invalid(format!(
                    "limit {name} must be in {}..={}, got {v}",
                    range.start(),
                    range.end()
                )));
            }
        }
        check_plain_name("profile", &self.profile)?;
        check_plain_name("verification_profile", &self.verification_profile)?;
        if self.verification_profile.contains('@') {
            return Err(ContractError::Invalid(format!(
                "verification_profile {:?} must not contain '@'",
                self.verification_profile
            )));
        }
        if let Some(d) = &self.profile_digest {
            let hex = d.len() == 64 && d.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
            if !hex {
                return Err(ContractError::Invalid(format!(
                    "profile_digest {d:?} must be 64 lowercase hex characters"
                )));
            }
        }
        if let Some(d) = &self.guest_image_digest {
            let hex = d.len() == 64 && d.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
            if !hex {
                return Err(ContractError::Invalid(format!(
                    "guest_image_digest {d:?} must be 64 lowercase hex characters"
                )));
            }
        }
        if self.editable_paths.is_empty() {
            return Err(ContractError::Invalid(
                "editable_paths must not be empty".into(),
            ));
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
        path_matches(&self.editable_paths, rel)
    }
}

/// True if `rel` (a repo-relative path) matches one of `patterns`. `dir/**` matches anything
/// under `dir/`; any other pattern matches exactly. Absolute paths and `..` never match.
pub fn path_matches(patterns: &[String], rel: &str) -> bool {
    if rel.starts_with('/') || has_parent_component(rel) {
        return false;
    }
    patterns.iter().any(|pat| match pat.strip_suffix("**") {
        Some(prefix) if prefix.ends_with('/') => rel.starts_with(prefix),
        _ => rel == pat,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    const OK: &str = r#"{"goal":"g","repository":{"source":"/r","revision":"abc"},"profile":"python-stdlib-v1",
      "editable_paths":["src/**"],"verification_profile":"parser-checks-v1",
      "capabilities":["snapshot.read","workspace.apply_patch","verification.run","artifact.export"],
      "limits":{"model_requests":12,"max_output_tokens_per_request":4096,"tool_actions":50,
                "deadline_seconds":1200,"worker_vcpus":2,"worker_memory_mib":2048}}"#;

    #[test]
    fn model_request_capability_parses_and_serializes_as_model_dot_request() {
        let j = OK.replace(
            "\"artifact.export\"]",
            "\"artifact.export\",\"model.request\"]",
        );
        assert_ne!(j, OK);
        let c = Contract::parse(&j).unwrap();
        assert!(c.capabilities.contains(&Capability::ModelRequest));
        assert_eq!(
            serde_json::to_value(Capability::ModelRequest).unwrap(),
            serde_json::json!("model.request")
        );
        let ok = Contract::parse(OK).unwrap();
        let out = serde_json::to_string(&ok).unwrap();
        assert_eq!(
            serde_json::to_string(&Contract::parse(&out).unwrap()).unwrap(),
            out
        );
    }

    /// A contract without the optional VM resource fields serializes exactly as it did before
    /// they existed, so every stored contract keeps its digest (pinned before the change).
    #[test]
    fn a_contract_without_resource_fields_keeps_its_digest() {
        let c = Contract::parse(OK).unwrap();
        let digest = crate::ids::Digest::of(&serde_json::to_vec(&c).unwrap());
        assert_eq!(
            digest.to_string(),
            "4ef04e21c7f6cbe572fa7a2ba3102f720ddc45460a90dc76569f0818e22d0062"
        );
    }

    fn with_limits(extra: &str) -> String {
        OK.replace(
            "\"worker_memory_mib\":2048}",
            &format!("\"worker_memory_mib\":2048,{extra}}}"),
        )
    }

    #[test]
    fn vm_resource_fields_round_trip_and_are_omitted_when_absent() {
        let j = with_limits(
            r#""worker_disk_mib":2048,"worker_scratch_mib":768,"worker_disk_bandwidth_mib_s":64,"worker_disk_iops":5000"#,
        );
        let c = Contract::parse(&j).unwrap();
        assert_eq!(c.limits.worker_disk_mib, Some(2048));
        assert_eq!(c.limits.worker_scratch_mib, Some(768));
        assert_eq!(c.limits.worker_disk_bandwidth_mib_s, Some(64));
        assert_eq!(c.limits.worker_disk_iops, Some(5000));
        let out = serde_json::to_string(&c).unwrap();
        assert_eq!(Contract::parse(&out).unwrap(), c);
        let plain = serde_json::to_string(&Contract::parse(OK).unwrap()).unwrap();
        assert!(!plain.contains("worker_disk"), "{plain}");
        assert!(!plain.contains("worker_scratch"), "{plain}");
    }

    #[test]
    fn vm_resource_fields_are_bounded() {
        for (field, lo, hi) in [
            ("worker_disk_mib", 512u64, 32768u64),
            ("worker_scratch_mib", 384, 32768),
            ("worker_disk_bandwidth_mib_s", 32, 4096),
            ("worker_disk_iops", 5000, 1_000_000),
        ] {
            for ok in [lo, hi] {
                Contract::parse(&with_limits(&format!("\"{field}\":{ok}")))
                    .unwrap_or_else(|e| panic!("{field}={ok}: {e}"));
            }
            for bad in [lo - 1, hi + 1, u64::from(u32::MAX)] {
                let err = Contract::parse(&with_limits(&format!("\"{field}\":{bad}")))
                    .expect_err(&format!("{field}={bad} accepted"))
                    .to_string();
                assert!(err.contains(field), "{err}");
                assert!(err.contains(&format!("{lo}..={hi}")), "{err}");
            }
            assert!(Contract::parse(&with_limits(&format!("\"{field}\":4294967296"))).is_err());
        }
    }

    #[test]
    fn parses_spec_example() {
        assert!(Contract::parse(OK).is_ok());
    }
    #[test]
    fn rejects_unknown_field() {
        let j = OK.replacen("{\"goal\"", "{\"extra\":1,\"goal\"", 1);
        assert!(Contract::parse(&j).is_err());
    }
    #[test]
    fn rejects_zero_limit() {
        assert!(Contract::parse(&OK.replace("\"tool_actions\":50", "\"tool_actions\":0")).is_err());
    }
    #[test]
    fn rejects_empty_editable_paths() {
        assert!(Contract::parse(&OK.replace("[\"src/**\"]", "[]")).is_err());
    }
    #[test]
    fn rejects_escaping_glob() {
        assert!(Contract::parse(&OK.replace("src/**", "../x/**")).is_err());
        assert!(Contract::parse(&OK.replace("src/**", "/etc/**")).is_err());
    }
    #[test]
    fn rejects_profile_ids_that_are_not_one_plain_name() {
        for bad in [
            "../../tmp/x",
            "/abs/path",
            "a/b",
            "..",
            "",
            ".",
            "-rf",
            "a\\b",
            "a\u{0}b",
            "x/",
            "./x",
        ] {
            let id = serde_json::to_string(bad).unwrap();
            let vp = OK.replace(
                "\"verification_profile\":\"parser-checks-v1\"",
                &format!("\"verification_profile\":{id}"),
            );
            assert_ne!(vp, OK);
            let err = Contract::parse(&vp).unwrap_err().to_string();
            assert!(err.contains("verification_profile"), "{bad:?}: {err}");
            let p = OK.replace(
                "\"profile\":\"python-stdlib-v1\"",
                &format!("\"profile\":{id}"),
            );
            assert_ne!(p, OK);
            assert!(
                Contract::parse(&p)
                    .unwrap_err()
                    .to_string()
                    .contains("profile"),
                "{bad:?}"
            );
        }
        for good in ["parser-checks-v1", "p", "a.b_c-2"] {
            let id = serde_json::to_string(good).unwrap();
            assert!(
                Contract::parse(&OK.replace("\"parser-checks-v1\"", &id)).is_ok(),
                "{good:?}"
            );
        }
    }
    #[test]
    fn path_allowed_matches_glob_and_blocks_traversal() {
        let c = Contract::parse(OK).unwrap();
        assert!(c.path_allowed("src/parser.py"));
        assert!(!c.path_allowed("tests/test_parser.py"));
        assert!(!c.path_allowed("src/../tests/x.py"));
    }
    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    fn with_digest(d: &str) -> String {
        OK.replace(
            "\"verification_profile\"",
            &format!("\"profile_digest\":\"{d}\",\"verification_profile\""),
        )
    }
    #[test]
    fn profile_digest_must_be_64_hex_when_present() {
        let c = Contract::parse(&with_digest(DIGEST)).unwrap();
        assert_eq!(c.profile_digest.as_deref(), Some(DIGEST));
        for bad in [
            "",
            "abc",
            &DIGEST[..63],
            &format!("{DIGEST}0"),
            &DIGEST.to_uppercase(),
            &format!("{}g", &DIGEST[..63]),
        ] {
            let err = Contract::parse(&with_digest(bad)).unwrap_err().to_string();
            assert!(err.contains("profile_digest"), "{bad:?}: {err}");
        }
    }
    #[test]
    fn contract_without_profile_digest_parses_and_reserializes_byte_identically() {
        let c = Contract::parse(OK).unwrap();
        assert!(c.profile_digest.is_none());
        let out = serde_json::to_string(&c).unwrap();
        assert!(!out.contains("profile_digest"));
        let again = Contract::parse(&out).unwrap();
        assert_eq!(serde_json::to_string(&again).unwrap(), out);
        let with = serde_json::to_string(&Contract::parse(&with_digest(DIGEST)).unwrap()).unwrap();
        assert!(with.contains("profile_digest"));
    }
    #[test]
    fn verification_profile_with_at_sign_is_rejected() {
        let err = Contract::parse(&OK.replace("parser-checks-v1", "parser@checks"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("verification_profile") && err.contains('@'),
            "{err}"
        );
    }
    #[test]
    fn path_matches_is_shared_with_path_allowed() {
        let pats = vec!["src/**".to_string(), "README.md".to_string()];
        let table = [
            ("src/a/b.py", true),
            ("README.md", true),
            ("src/../x", false),
            ("/src/a", false),
            ("srcfoo/x", false),
            ("src", false),
            ("README.mdx", false),
            ("tests/x", false),
        ];
        let c = Contract::parse(&OK.replace("[\"src/**\"]", "[\"src/**\",\"README.md\"]")).unwrap();
        for (p, want) in table {
            assert_eq!(path_matches(&pats, p), want, "{p}");
            assert_eq!(c.path_allowed(p), want, "{p}");
        }
    }
    #[test]
    fn guest_image_digest_must_be_64_hex_when_present() {
        let with = |d: &str| {
            OK.replace(
                "\"verification_profile\"",
                &format!("\"guest_image_digest\":\"{d}\",\"verification_profile\""),
            )
        };
        let c = Contract::parse(&with(DIGEST)).unwrap();
        assert_eq!(c.guest_image_digest.as_deref(), Some(DIGEST));
        for bad in [
            "",
            "abc",
            &DIGEST[..63],
            &format!("{DIGEST}0"),
            &DIGEST.to_uppercase(),
            &format!("{}g", &DIGEST[..63]),
        ] {
            let err = Contract::parse(&with(bad)).unwrap_err().to_string();
            assert!(err.contains("guest_image_digest"), "{bad:?}: {err}");
        }
    }
    #[test]
    fn contract_without_guest_image_digest_reserializes_byte_identically() {
        let c = Contract::parse(OK).unwrap();
        assert!(c.guest_image_digest.is_none());
        let out = serde_json::to_string(&c).unwrap();
        assert!(!out.contains("guest_image_digest"));
        assert_eq!(
            out,
            r#"{"goal":"g","repository":{"source":"/r","revision":"abc"},"profile":"python-stdlib-v1","editable_paths":["src/**"],"verification_profile":"parser-checks-v1","capabilities":["snapshot.read","workspace.apply_patch","verification.run","artifact.export"],"limits":{"model_requests":12,"max_output_tokens_per_request":4096,"tool_actions":50,"deadline_seconds":1200,"worker_vcpus":2,"worker_memory_mib":2048}}"#
        );
    }
}
