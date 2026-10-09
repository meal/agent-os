# Agent session on the microVM (KVM tier)

Branch `acceptance-evidence`, tests at `fff1c23`, guest fix at `aba71fc`. Image
`agent-cli-py314-v1` (protocol 2, Claude Code 2.1.295 native binary, tarball sha256
`d45a2fa14a7ea0b8ab4f56e502de5893c2b816078c71c4d8ab2e4142148f3d95`), final digest
`d5fac431023288f24639194fee32ad5d95a36724eb51f4487ef58f37602d8d3b` (the first build at
`aec9ec6` was `f704479a…`; the guest changed after it). No real model API was called: the model
is the repo's fake provider, scripted per test.

## Commands (all with `COMPOSE_PROJECT_NAME=agentos-acceptance`, image from the `guest-images` volume)

1. `docker compose run --rm test-kvm sh scripts/build-guest-image.sh guest/agent-cli-py314-v1
   build/guest-images/agent-cli-py314-v1 --verify` -> "two builds are byte-identical".
2. `... -e AGENTOS_GUEST_IMAGE=/work/build/guest-images/agent-cli-py314-v1 test-kvm cargo test -p
   agentos-engine --locked --test kvm_agent -- --nocapture` -> 3 passed in 72.2 s.
3. The same environment with `cargo test --workspace --locked --no-fail-fast` -> exit 0, 60
   test binaries ok, no failure.
4. As 3 with `-e AGENTOS_TEST_WORKER=firecracker` -> exit 0, 60 ok, no failure.

Host disk: 79 GB free before, 78 GB after (85 % used).

## The three tests (`crates/agentos-engine/tests/kvm_agent.rs`)

- `no_nic_no_key_and_lo_down_after_the_session`: a scripted shell session inside the VM; its
  observations come back in the patch and the test asserts them: a non-loopback connection
  fails, the environment holds no secret beyond the literal placeholder key, `lo` is down again
  afterwards, and the existing `hostile/net-probe` profile still fails every connection in the
  verification that follows. Passed.
- `the_real_cli_fixes_the_fixture_and_the_task_succeeds`: the real `/opt/agent-cli/claude`,
  argv `-p <goal> --permission-mode bypassPermissions --disallowedTools WebFetch WebSearch`,
  the fake provider answering a `Bash` `sed -i` tool call and then `end_turn`; the patch goes
  through ApplyPatch and protected verification and the task SUCCEEDED. Passed.
- `cancel_and_lease_end_the_real_cli_session_and_leave_nothing`: cancel marker: outcome "agent
  session cancelled", returned 70 ms after the cancel; lease expiry: "agent session timed out at
  its lease", returned after 60.07 s; in both the VM is gone and no Firecracker or session
  process is left. Passed.

## What the real VM showed

- `lo` is brought up for the session and down again after it; a controller `PATH` was missing,
  so the CLI's shell could not find `sed` (exit 127): fixed by the guest setting
  `PATH=/usr/bin:/bin` itself, which a message cannot override (`aba71fc`, unit test).
- The chown handover, git under split uids and running the CLI as uid 1001 work.
- Limits: the existing check limits sufficed (RLIMIT_NPROC 256, RLIMIT_NOFILE 1024). With a
  1024 MiB guest the CLI peaked at VmHWM 231 740 kB with 8 threads, 12 threads for uid 1001 in
  total. 512 MiB was not tried.
- The Task 8 explanation for late process cleanup (orphans reaped asynchronously by the
  container init) does **not** hold on KVM: grandchildren (`sleep 600`, `setsid sleep 601`) were
  alive in the VM before the cancel and gone with the VM afterwards.

## Not shown

- A live API run (Task 10, billed, awaiting the owner's decision).
- Whether the live API accepts the forwarded request body once `context_management`,
  `safeguards` and `output_config` are stripped and no `anthropic-beta` header is sent.
- One unexplained single failure of the old timing test
  `a_readable_job_is_waited_for_until_its_own_lease_plus_grace_whatever_the_bound` in an earlier
  incomplete run (119 us instead of about 1 s); it passed 13 times alone and in full binaries,
  and in the final runs above.
