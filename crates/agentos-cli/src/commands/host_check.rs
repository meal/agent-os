//! `version` and `host-check`: what this build speaks, and whether this host can run its
//! Firecracker worker (the installer runs the staged release's own check before activating
//! it).

use std::fs::OpenOptions;
use std::process::Command;

use agentos_component::RUNTIME;
use agentos_core::guest::GUEST_PROTOCOL;
use agentos_core::resources::VM_RESOURCES_VERSION;
use agentos_engine::firecracker::{FIRECRACKER_VERSION_PREFIX, firecracker_version};
use agentos_engine::model::policy::{LIMITS_VERSION, POLICY_VERSION};
use serde_json::json;

use super::component::ANALYZER_WORLD;
use super::print;
use crate::error::CliError;
use crate::home::Home;

pub fn version() -> Result<(), CliError> {
    print(&json!({
        "agentos": env!("CARGO_PKG_VERSION"),
        "guest_protocol": GUEST_PROTOCOL,
        "model_policy_version": POLICY_VERSION,
        "model_limits_version": LIMITS_VERSION,
        "vm_resources_version": VM_RESOURCES_VERSION,
        "analyzer_runtime": RUNTIME,
        "analyzer_world": ANALYZER_WORLD,
        "firecracker": FIRECRACKER_VERSION_PREFIX,
    }));
    Ok(())
}

fn reason(r: Result<(), String>) -> (bool, String) {
    match r {
        Ok(()) => (true, "ok".into()),
        Err(why) => (false, why),
    }
}

pub fn host_check(home: &Home) -> Result<(), CliError> {
    let kvm = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .map(drop)
        .map_err(|e| format!("/dev/kvm: {e}"));
    let fc_bin = home.firecracker_bin();
    let firecracker = firecracker_version(&fc_bin)
        .map_err(|e| format!("{}: {e}", fc_bin.display()))
        .and_then(|v| {
            if v.starts_with(FIRECRACKER_VERSION_PREFIX) {
                Ok(())
            } else {
                Err(format!(
                    "{}: expected {FIRECRACKER_VERSION_PREFIX}, got {v}",
                    fc_bin.display()
                ))
            }
        });
    let git = match Command::new("git").arg("--version").output() {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => Err(format!("git --version exited with {}", out.status)),
        Err(e) => Err(format!("git: {e}")),
    };
    let jail = home.probe_jail()?;
    let jail_required = !home.allow_unjailed;
    let (kvm_ok, kvm) = reason(kvm);
    let (fc_ok, firecracker) = reason(firecracker);
    let (git_ok, git) = reason(git);
    let (jail_ok, jail) = reason(jail);
    print(&json!({
        "kvm": kvm, "firecracker": firecracker, "git": git,
        "jail": jail, "jail_required": jail_required,
    }));
    if kvm_ok && fc_ok && git_ok && (jail_ok || !jail_required) {
        Ok(())
    } else {
        Err(CliError::other("host check failed"))
    }
}
