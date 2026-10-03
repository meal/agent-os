//! `agentos-guest exec-check`: the trampoline the VM agent starts the check through (spec
//! issue 6). The agent starts it as root; it sets the OOM priority while still privileged,
//! the rlimits, drops to `--uid/--gid` with no supplementary groups, sets `no_new_privs`, and
//! `exec`s the program. The compose `test` service runs as root, so the drop is real here.

use std::process::Command;

const UID: &str = "1001";

fn is_root() -> bool {
    rustix::process::geteuid().is_root()
}

fn skip_unless_root() -> bool {
    if !is_root() {
        println!("SKIPPED: the trampoline drops privileges and must start as root (the compose test service is root)");
        return true;
    }
    false
}

fn limits_args() -> Vec<&'static str> {
    vec!["--uid", UID, "--gid", UID, "--nproc", "256", "--nofile", "1024", "--oom", "1000", "--"]
}

fn exec_check(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_agentos-guest")).arg("exec-check").args(args).output().unwrap()
}

fn exec_check_sh(script: &str) -> std::process::Output {
    let mut args = limits_args();
    args.extend(["sh", "-c", script, "the-arg"]);
    exec_check(&args)
}

/// The `(soft, hard)` columns of one `/proc/self/limits` row.
fn limit(limits: &str, name: &str) -> (String, String) {
    let line = limits.lines().find(|l| l.starts_with(name)).unwrap_or_else(|| panic!("no {name} in {limits}"));
    let fields: Vec<&str> = line[name.len()..].split_whitespace().collect();
    (fields[0].to_string(), fields[1].to_string())
}

/// Whether this process holds `CAP_SYS_RESOURCE` (bit 24 of `CapEff`).
fn has_cap_sys_resource() -> bool {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let hex = status.lines().find_map(|l| l.strip_prefix("CapEff:")).unwrap().trim();
    u64::from_str_radix(hex, 16).unwrap() >> 24 & 1 == 1
}

#[test]
fn trampoline_applies_rlimits_and_oom_score_then_execs() {
    if skip_unless_root() {
        return;
    }
    // dash has no `ulimit -u`; `/proc/self/limits` of the exec'd shell is shell-independent.
    let out = exec_check_sh("cat /proc/self/limits; echo oom=$(cat /proc/self/oom_score_adj); echo args=\"$0\"");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(limit(&stdout, "Max processes"), ("256".into(), "256".into()), "{stdout}");
    assert_eq!(limit(&stdout, "Max open files"), ("1024".into(), "1024".into()), "{stdout}");
    assert!(stdout.contains("oom=1000\n"), "{stdout}");
    // The program's own arguments pass through untouched.
    assert!(stdout.contains("args=the-arg\n"), "{stdout}");
}

#[test]
fn the_check_cannot_lower_its_oom_score_adj() {
    if skip_unless_root() {
        return;
    }
    let try_write = |v: &str| {
        let out = exec_check_sh(&format!("echo {v} > /proc/self/oom_score_adj && echo wrote || echo refused; cat /proc/self/oom_score_adj"));
        assert!(out.status.success(), "{out:?}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    // Below any floor this process could have inherited: always refused.
    assert_eq!(try_write("-1000"), "refused\n1000\n");
    if has_cap_sys_resource() {
        // The trampoline wrote 1000 with CAP_SYS_RESOURCE (as the VM agent does): that is
        // now the floor, and nothing below it can be written back.
        for v in ["0", "999"] {
            assert_eq!(try_write(v), "refused\n1000\n", "{v}");
        }
    } else {
        println!(
            "NOTE: no CAP_SYS_RESOURCE in this container, so the trampoline cannot set the 1000 floor here; \
             the KVM tier asserts it (hostile mem-hog that first writes -1000)"
        );
    }
}

#[test]
fn the_check_runs_as_the_given_ids_without_groups_and_cannot_regain_root() {
    if skip_unless_root() {
        return;
    }
    let out = exec_check_sh(
        "id -u; id -g; id -G; grep -E '^(Uid|Gid|Groups|NoNewPrivs):' /proc/self/status; \
         python3 -c 'import os; os.setuid(0)' 2>/dev/null && echo regained || echo no-root",
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    let lines: Vec<String> = stdout.lines().map(|l| l.split_whitespace().collect::<Vec<_>>().join(" ")).collect();
    assert_eq!(
        lines,
        [
            "1001",
            "1001",
            "1001",
            "Uid: 1001 1001 1001 1001",
            "Gid: 1001 1001 1001 1001",
            "Groups:",
            "NoNewPrivs: 1",
            "no-root",
        ],
        "{stdout}"
    );
}

#[test]
fn trampoline_execs_in_place_keeping_the_environment_it_was_given() {
    if skip_unless_root() {
        return;
    }
    let out = Command::new(env!("CARGO_BIN_EXE_agentos-guest"))
        .arg("exec-check")
        .args(limits_args())
        .args(["sh", "-c", "echo \"$AGENTOS_TRAMPOLINE_PROBE\"; tr '\\0' ' ' < /proc/self/cmdline"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("AGENTOS_TRAMPOLINE_PROBE", "kept")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(stdout.starts_with("kept\n"), "{stdout}");
    // exec, not spawn: the process is the shell itself, not a child of the trampoline.
    assert!(stdout.contains("sh -c"), "{stdout}");
    assert!(!stdout.contains("exec-check"), "{stdout}");
}

#[test]
fn trampoline_reports_a_missing_program_with_exit_127() {
    if skip_unless_root() {
        return;
    }
    let mut args = limits_args();
    args.extend(["/nonexistent/prog", "x"]);
    let out = exec_check(&args);
    assert_eq!(out.status.code(), Some(127), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("/nonexistent/prog"), "{stderr}");
}

#[test]
fn trampoline_refuses_malformed_arguments_with_exit_2() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("ran");
    let touch = format!("touch {}", marker.display());
    let ok = ["--uid", "1001", "--gid", "1001", "--nproc", "256", "--nofile", "1024", "--oom", "1000"];
    let with = |opts: &[&str], tail: &[&str]| -> Vec<String> { opts.iter().chain(tail).map(|s| s.to_string()).collect() };
    let run = ["--", "sh", "-c", touch.as_str()];
    let cases: Vec<Vec<String>> = vec![
        vec![],
        with(&ok, &[]),
        with(&ok, &["--"]),
        with(&["--uid", "1001", "--gid", "1001", "--nproc", "x", "--nofile", "1024", "--oom", "1000"], &run),
        with(&["--uid", "1001", "--gid", "1001", "--nproc", "256", "--nofile", "1024"], &run),
        with(&["--uid", "1001", "--gid", "1001", "--nproc", "256", "--nproc", "256", "--nofile", "1024", "--oom", "1000"], &run),
        with(&["--uid", "1001", "--gid", "1001", "--nproc", "256", "--nofile", "1024", "--oom", "1001"], &run),
        with(&["--uid", "1001", "--gid", "1001", "--nproc", "256", "--nofile", "1024", "--oom", "-1001"], &run),
        with(&ok, &["--bogus", "1", "--", "true"]),
        with(&ok, &["true"]),
        // The ids: missing, malformed, or root (no drop at all).
        with(&["--gid", "1001", "--nproc", "256", "--nofile", "1024", "--oom", "1000"], &run),
        with(&["--uid", "1001", "--nproc", "256", "--nofile", "1024", "--oom", "1000"], &run),
        with(&["--uid", "check", "--gid", "1001", "--nproc", "256", "--nofile", "1024", "--oom", "1000"], &run),
        with(&["--uid", "1001", "--gid", "-1", "--nproc", "256", "--nofile", "1024", "--oom", "1000"], &run),
        with(&["--uid", "0", "--gid", "1001", "--nproc", "256", "--nofile", "1024", "--oom", "1000"], &run),
        with(&["--uid", "1001", "--gid", "0", "--nproc", "256", "--nofile", "1024", "--oom", "1000"], &run),
    ];
    for args in &cases {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = exec_check(&args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("usage: agentos-guest exec-check"), "{args:?}: {stderr}");
        assert!(out.stdout.is_empty(), "{args:?}");
        assert!(!marker.exists(), "{args:?} ran the program");
    }
}

#[test]
fn a_trampoline_that_cannot_drop_privileges_runs_nothing() {
    if skip_unless_root() {
        return;
    }
    use std::os::unix::process::CommandExt;
    // Started unprivileged (another uid), it cannot clear its groups or become 1001.
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("ran");
    std::fs::set_permissions(dir.path(), std::os::unix::fs::PermissionsExt::from_mode(0o777)).unwrap();
    let touch = format!("touch {}", marker.display());
    let out = Command::new(env!("CARGO_BIN_EXE_agentos-guest"))
        .arg("exec-check")
        .args(limits_args())
        .args(["sh", "-c", touch.as_str()])
        .uid(1002)
        .gid(1002)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(126), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot drop privileges"), "{out:?}");
    assert!(!marker.exists(), "the program ran without the drop");
}
