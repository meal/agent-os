//! `agentos-guest exec-check`: the trampoline the VM agent starts the check through (spec
//! issue 6). It runs on the host here: lowering rlimits and raising `oom_score_adj` need no
//! privilege, so the effects are visible in the exec'd program's own `/proc/self`.

use std::process::Command;

fn exec_check(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_agentos-guest")).arg("exec-check").args(args).output().unwrap()
}

/// The `(soft, hard)` columns of one `/proc/self/limits` row.
fn limit(limits: &str, name: &str) -> (String, String) {
    let line = limits.lines().find(|l| l.starts_with(name)).unwrap_or_else(|| panic!("no {name} in {limits}"));
    let fields: Vec<&str> = line[name.len()..].split_whitespace().collect();
    (fields[0].to_string(), fields[1].to_string())
}

#[test]
fn trampoline_applies_rlimits_and_oom_score_then_execs() {
    // dash has no `ulimit -u`; `/proc/self/limits` of the exec'd shell is shell-independent.
    let out = exec_check(&[
        "--nproc",
        "256",
        "--nofile",
        "1024",
        "--oom",
        "1000",
        "--",
        "sh",
        "-c",
        "cat /proc/self/limits; echo oom=$(cat /proc/self/oom_score_adj); echo pid=$$; echo args=\"$0\"",
        "the-arg",
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(limit(&stdout, "Max processes"), ("256".into(), "256".into()), "{stdout}");
    assert_eq!(limit(&stdout, "Max open files"), ("1024".into(), "1024".into()), "{stdout}");
    assert!(stdout.contains("oom=1000\n"), "{stdout}");
    // The program's own arguments pass through untouched.
    assert!(stdout.contains("args=the-arg\n"), "{stdout}");
}

#[test]
fn trampoline_execs_in_place_keeping_the_environment_it_was_given() {
    let out = Command::new(env!("CARGO_BIN_EXE_agentos-guest"))
        .args(["exec-check", "--nproc", "256", "--nofile", "1024", "--oom", "1000", "--"])
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
    let out = exec_check(&["--nproc", "256", "--nofile", "1024", "--oom", "1000", "--", "/nonexistent/prog", "x"]);
    assert_eq!(out.status.code(), Some(127), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("/nonexistent/prog"), "{stderr}");
}

#[test]
fn trampoline_refuses_malformed_arguments_with_exit_2() {
    let cases: &[&[&str]] = &[
        &[],
        &["--nproc", "256", "--nofile", "1024", "--oom", "1000"],
        &["--nproc", "256", "--nofile", "1024", "--oom", "1000", "--"],
        &["--nproc", "x", "--nofile", "1024", "--oom", "1000", "--", "true"],
        &["--nproc", "256", "--nofile", "1024", "--", "true"],
        &["--nproc", "256", "--nproc", "256", "--nofile", "1024", "--oom", "1000", "--", "true"],
        &["--nproc", "256", "--nofile", "1024", "--oom", "1001", "--", "true"],
        &["--nproc", "256", "--nofile", "1024", "--oom", "-1001", "--", "true"],
        &["--nproc", "256", "--nofile", "1024", "--oom", "1000", "--bogus", "1", "--", "true"],
        &["--nproc", "256", "--nofile", "1024", "--oom", "1000", "true"],
    ];
    for args in cases {
        let out = exec_check(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("usage: agentos-guest exec-check"), "{args:?}: {stderr}");
        assert!(out.stdout.is_empty(), "{args:?}");
    }
}
