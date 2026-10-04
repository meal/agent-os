//! Firecracker processes as `/proc` shows them, and the VM ids a home used. Self-contained
//! (no `super::` items): the CLI tests include this file with
//! `#[path = "../../agentos-engine/tests/common/procs.rs"] mod procs;`.
#![allow(dead_code)]

use std::fs;
use std::path::Path;

/// Whether `pid` is a live (non-zombie) process.
fn live(pid: i32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| {
        s.rsplit_once(") ")
            .is_some_and(|(_, rest)| !rest.starts_with('Z'))
    })
}

fn all_pids() -> Vec<i32> {
    fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse().ok())
        .collect()
}

/// A live `firecracker` process (`comm`), as `/proc` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FcProc {
    pub pid: i32,
    /// The real uid (`Uid:` of `/proc/<pid>/status`).
    pub uid: u32,
    /// The cgroup v2 line of `/proc/<pid>/cgroup` (`0::/agentos/<id>`).
    pub cgroup: String,
    pub cmdline: Vec<String>,
}

impl FcProc {
    /// The value of `--id`: the attempt id, or `inspect-<uuid>`.
    pub fn id(&self) -> Option<&str> {
        self.cmdline
            .iter()
            .position(|a| a == "--id")
            .and_then(|i| self.cmdline.get(i + 1))
            .map(String::as_str)
    }
}

/// Every live (non-zombie) `firecracker` process. A jailed one's command line is
/// `/firecracker --id <id> … --config-file /vm.json` and names no host path, so a home's
/// processes are told apart by `--id` (`home_firecrackers`), never by `processes_naming`.
pub fn firecracker_processes() -> Vec<FcProc> {
    all_pids()
        .into_iter()
        .filter_map(|pid| {
            let comm = fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
            if comm.trim_end() != "firecracker" || !live(pid) {
                return None;
            }
            let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
            let uid = status
                .lines()
                .find(|l| l.starts_with("Uid:"))?
                .split_whitespace()
                .nth(1)?
                .parse()
                .ok()?;
            let cgroup = fs::read_to_string(format!("/proc/{pid}/cgroup"))
                .ok()?
                .lines()
                .find(|l| l.starts_with("0::"))?
                .to_string();
            let cmdline = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
            let cmdline = cmdline
                .split(|b| *b == 0)
                .filter(|a| !a.is_empty())
                .map(|a| String::from_utf8_lossy(a).into_owned())
                .collect();
            Some(FcProc {
                pid,
                uid,
                cgroup,
                cmdline,
            })
        })
        .collect()
}

/// The VM ids a home under `root` ever used: the attempt id of every `<root>/jobs/<effect>-
/// <attempt>` and `inspect-<uuid>` of every `<root>/inspect/<task>/<uuid>`.
pub fn home_vm_ids(root: &Path) -> Vec<String> {
    let names = |dir: &Path| -> Vec<String> {
        fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .collect()
    };
    let mut ids: Vec<String> = names(&root.join("jobs"))
        .into_iter()
        .filter_map(|n| n.get(65..).map(str::to_string))
        .collect();
    for task in names(&root.join("inspect")) {
        ids.extend(
            names(&root.join("inspect").join(task))
                .into_iter()
                .map(|u| format!("inspect-{u}")),
        );
    }
    ids
}

/// The live Firecracker processes of the home under `root` (`home_vm_ids`).
pub fn home_firecrackers(root: &Path) -> Vec<FcProc> {
    let ids = home_vm_ids(root);
    firecracker_processes()
        .into_iter()
        .filter(|p| p.id().is_some_and(|id| ids.iter().any(|i| i == id)))
        .collect()
}
