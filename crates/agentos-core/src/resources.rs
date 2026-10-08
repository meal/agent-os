//! A task's resolved VM resources: what its contract asked for, with the defaults of a
//! resources version applied. `submit` records them in `Submitted.vm_resources`; every later
//! command resolves the stored contract again and must get the same value. A journal without
//! the record predates it and runs with the frozen version-0 values.

use serde::{Deserialize, Serialize};

use crate::contract::Limits;

/// The version `resolve` applies. A change to any default is a new version.
pub const VM_RESOURCES_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmResources {
    pub version: u32,
    /// The workspace drive (`ws.img`).
    pub disk_mib: u32,
    /// The scratch drive (`scratch.img`).
    pub scratch_mib: u32,
    /// Per writable drive; `None` is unlimited.
    pub bandwidth_mib_s: Option<u32>,
    /// Per writable drive; `None` is unlimited.
    pub iops: Option<u32>,
}

impl VmResources {
    /// What every task submitted before resources were recorded ran with: the 1 GiB workspace
    /// and 512 MiB scratch images, no rate limits. Frozen: never change these values.
    pub const V0: VmResources = VmResources {
        version: 0,
        disk_mib: 1024,
        scratch_mib: 512,
        bandwidth_mib_s: None,
        iops: None,
    };

    /// `limits` with version 1's defaults (the same sizes as version 0) for absent fields.
    pub fn resolve(limits: &Limits) -> VmResources {
        VmResources {
            version: VM_RESOURCES_VERSION,
            disk_mib: limits.worker_disk_mib.unwrap_or(1024),
            scratch_mib: limits.worker_scratch_mib.unwrap_or(512),
            bandwidth_mib_s: limits.worker_disk_bandwidth_mib_s,
            iops: limits.worker_disk_iops,
        }
    }

    /// The resources recorded in a `Submitted` payload, or [`VmResources::V0`] when it has none.
    pub fn from_submitted(payload: &serde_json::Value) -> Result<VmResources, String> {
        match payload.get("vm_resources") {
            None | Some(serde_json::Value::Null) => Ok(VmResources::V0),
            Some(v) => serde_json::from_value(v.clone())
                .map_err(|e| format!("Submitted.vm_resources is malformed: {e}")),
        }
    }

    /// `disk_mib` in bytes (a `u32` of MiB always fits a `u64` of bytes).
    pub fn disk_bytes(&self) -> u64 {
        u64::from(self.disk_mib) << 20
    }

    /// `scratch_mib` in bytes.
    pub fn scratch_bytes(&self) -> u64 {
        u64::from(self.scratch_mib) << 20
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::Contract;
    use crate::guest::{SCRATCH_IMAGE_BYTES, WS_IMAGE_BYTES};

    fn limits(extra: &str) -> Limits {
        let json = format!(
            r#"{{"goal":"g","repository":{{"source":"/r","revision":"abc"}},"profile":"p",
            "editable_paths":["src/**"],"verification_profile":"v","capabilities":[],
            "limits":{{"model_requests":1,"max_output_tokens_per_request":1,"tool_actions":1,
            "deadline_seconds":1,"worker_vcpus":1,"worker_memory_mib":256{extra}}}}}"#
        );
        Contract::parse(&json).unwrap().limits
    }

    #[test]
    fn version_zero_is_what_tasks_ran_with_before_the_record() {
        assert_eq!(VmResources::V0.disk_bytes(), WS_IMAGE_BYTES);
        assert_eq!(VmResources::V0.scratch_bytes(), SCRATCH_IMAGE_BYTES);
        assert_eq!(
            (VmResources::V0.bandwidth_mib_s, VmResources::V0.iops),
            (None, None)
        );
    }

    #[test]
    fn absent_fields_resolve_to_version_one_defaults() {
        assert_eq!(
            VmResources::resolve(&limits("")),
            VmResources {
                version: 1,
                ..VmResources::V0
            }
        );
    }

    #[test]
    fn present_fields_are_used_as_given() {
        let r = VmResources::resolve(&limits(
            r#","worker_disk_mib":32768,"worker_scratch_mib":384,"worker_disk_bandwidth_mib_s":8,"worker_disk_iops":10"#,
        ));
        assert_eq!(
            r,
            VmResources {
                version: 1,
                disk_mib: 32768,
                scratch_mib: 384,
                bandwidth_mib_s: Some(8),
                iops: Some(10)
            }
        );
        assert_eq!(r.disk_bytes(), 32768 * 1024 * 1024);
    }

    #[test]
    fn a_submitted_record_wins_and_its_absence_means_version_zero() {
        let recorded = VmResources {
            version: 1,
            disk_mib: 2048,
            scratch_mib: 512,
            bandwidth_mib_s: Some(64),
            iops: None,
        };
        let payload = serde_json::json!({ "worker": "firecracker", "vm_resources": recorded });
        assert_eq!(VmResources::from_submitted(&payload), Ok(recorded));
        assert_eq!(
            VmResources::from_submitted(&serde_json::json!({ "worker": "firecracker" })),
            Ok(VmResources::V0)
        );
        let bad = serde_json::json!({ "vm_resources": { "version": 1 } });
        assert!(
            VmResources::from_submitted(&bad)
                .unwrap_err()
                .contains("malformed")
        );
    }
}
