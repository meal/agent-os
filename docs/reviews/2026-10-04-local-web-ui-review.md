# Local web UI evidence and branch review

Milestone: [design](../superpowers/specs/2026-10-04-local-web-ui-design.md) and [ten-task plan](../superpowers/plans/2026-10-04-local-web-ui.md). The owner approved native execution and requested commit/push on completion. Implementation uses the existing isolated `codex/v01-completion` branch; no merge or PR was requested.

## Implemented scope

The workbench creates recorded READY contracts, requires deliberate digest-bound approval, owns one driver independently of HTTP requests, shows current journal state and read-only patch/final verification views, and streams authorized session-scoped exports. Existing CLI adapters use the same typed operations and retain their JSON fields, errors and recovery behavior. No journal schema changed.

Implementation checkpoints: bounded store reads `d7adc87`; shared queries/collector `f8eef92`; session boundary `54c5a28`; result views `36b8477`; archive cache `5c1748e`; browser review `2fb1dac`; shared submission/control `c25bf9d`; owned runner/actions `7e81b2c`; contract forms/approval `5361133`; final workflow/launch/CI in the commit introducing this record.

## Verification evidence

- Each implementation task used behavioral RED/GREEN checks and a recorded completion gate.
- Full CLI package passed after service/control/form extraction: 112 CLI integration tests, 23 application unit tests and 13 real-process HTTP tests. Host and fake-worker/jail modes were run; the existing crash matrix and tamper/recovery assertions remain covered.
- Chromium: **12/12 host and 12/12 fake Firecracker checks**, offline, without provider credentials or KVM. Includes create/approve/review/download byte parity, local-file input, ordinary forms with JavaScript disabled, lost-tab lifetime, SIGKILL restart with explicit recovery and single effect completion, browser/CLI pause/resume, cancel, missing-key refusal, outdated forms, changed staged profiles, stale reads, escaping, integrity/size failures, old sessions/nonces and revoked cached exports.
- Desktop (1280×900) and narrow (390×844) rendered screenshots inspected: readable status/evidence, wrapped digests, visible keyboard focus and usable navigation. Generated images/logs remain in ignored `build/ui-evidence/`; CI uploads only screenshots and sanitized test output on failure.
- Compose configuration assertions passed: Linux host-network UI, source read-only, writable target/home, no ports/devices/additional capabilities; browser test container networking disabled.
- Final required four gates (`sh scripts/check.sh`) are run on the assembled tree: formatting, strict workspace Clippy, host workspace and fake-worker/fake-jail workspace tests. All four passed on 2026-10-05; no failures, no real-provider/KVM opt-in.

Browser RED identified duplicate New task links, missing post-run Export availability, ordinary-form CSRF extraction using a default GET request, and partial responses on native control submissions. Regressions now cover each corrected behavior. Archive checks compare every entry path and byte with CLI export, normalizing only manifest `generated_events`; the source repository remains unchanged.

## Implementation rulings

1. Baseline validation was rerun from an immutable `cbcf602` archive after overlapping edits. Its disposable root was made traversable (0755) to match the normal checkout for an unprivileged fixture. Cost if wrong: extra verification time only.
2. The bounded collector charges journal/contract metadata and duplicated patch output as well as blob reads to prevent secondary unbounded allocations. Cost if wrong: a near-limit web result may be refused earlier than the unbounded CLI.
3. Base layout/static styles moved from Task 4 into Task 3 to make the real dashboard usable. Cost if wrong: task sequencing only.
4. Signed download claims use a one-way session fingerprint instead of the cookie value, so the ticket cannot expose an HttpOnly credential. Internal change with no compatibility cost.
5. Referrer policy uses `same-origin` rather than the initial `no-referrer` implementation. Native Chromium form POSTs under no-referrer send Origin null; accepting those would weaken the exact-Origin boundary. Same-origin preserves the Origin and suppresses cross-origin referrers. Cost if wrong: the local server sees the referring local task/form URL. Confirmed through a failing browser regression and the [Fetch Origin algorithm](https://fetch.spec.whatwg.org/#append-a-request-origin-header).

A prior existing CLI wait-for-verification-start test timed out once during overlapping build/check execution; isolated and sequential full reruns passed. Its cause was not established, and no product code or timeout was relaxed to hide it.

## Dependencies and limits

Axum 0.8.9, Askama 0.16.1, HTMX 2.0.11, tar 0.4.46 and Playwright 1.63.0 were checked against official package/documentation sources before use; Cargo/Python lockfiles record resolution. Browser Python uses the existing pyenv Python 3.14.8 and a separate virtual environment. No Ruby runtime was introduced. HTMX source, hash and license are vendored. CI actions are pinned to checked v7.0.1 commits ([checkout](https://github.com/actions/checkout/releases/tag/v7.0.1), [upload-artifact](https://github.com/actions/upload-artifact/releases/tag/v7.0.1)).

Real provider and real KVM acceptance remain outside this UI evidence tier. Linux Compose host networking is the supported container launcher. The UI remains one local owner/home, no scheduler/queue, no remote deployment, and no automatic source patch application. Operational limits and lifecycle behavior are in [the UI guide](../ui.md).

## Final independent review

Pending fresh-context whole-branch review under the selected execution skill. No independent approval is claimed yet.
