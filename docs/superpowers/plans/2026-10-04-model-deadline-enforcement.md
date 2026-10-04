# Model Deadline Enforcement Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop an in-flight model request when its task deadline expires, preserving possibly billed requests as uncertain.

**Architecture:** Keep the existing provider and executor interfaces. Bound `ModelExecutor::run` by the remaining absolute task duration; route timeout to the existing unresolved/forfeit path. No journal schema change and no retry inside the provider.

**Tech Stack:** Rust 1.98.1; Tokio; current `ModelProvider`, `ExecOutcome`, and effect/usage store; Docker Compose.

**Spec:** `Agent_OS_v1_Build_Plan.md`, sections "Persistence and action semantics" and "Acceptance and failure tests"; `docs/superpowers/specs/2026-10-03-phase-4-real-agent-workflow-design.md`, section "Uncertain model requests"; confirmed finding in `docs/reviews/2026-10-04-repository-review.md`.

## Global Constraints

- Builds and tests use Docker Compose.
- Preserve one HTTP send per effect attempt and client-level retries disabled.
- A sent request interrupted without a usable response must retain its counted reservation as uncertain.
- Past-deadline requests must not call the provider.
- Keep `deadline_ts == 0` as "no task deadline".
- No co-author commits. Recheck online library versions before any dependency change; this fix needs no dependency change.
- This plan fixes deadline expiry. Prompt cancellation/revocation of HTTP requests and response byte limits are independent roadmap items.

## Review Focus

- Deadline already expired before execution: no send, no executed-call count; covered by existing `past_the_deadline_nothing_is_sent`.
- Deadline expires during a slow successful response: stop awaiting and return unresolved; covered by the preserved regression.
- Fractional remaining second: use the absolute timestamp precisely, avoiding a whole extra second; covered by helper tests below.
- Missing deadline: preserve existing successful call/retention behavior; covered by existing request helpers with `deadline_ts: 0`.
- Interrupted send loses response: no retained placeholder receipt and no released reservation; covered by the extra regression assertions and existing forfeit tests.

## Task 1: Bound model execution by the task deadline

**Files:**

- Modify: `crates/agentos-engine/src/model/executor.rs` (pre-send check and provider await).
- Create: `crates/agentos-engine/tests/review_deadline_probe.rs` using the exact already-run source in `docs/reviews/model-deadline-regression.rs.txt`.
- Test: existing `crates/agentos-engine/tests/model_executor.rs`, `recover_forfeit.rs`, `model_flow.rs`, and `model_crash_matrix.rs`.

**Interfaces:**

- Consumes unchanged `ModelProvider::complete(&self, body: &[u8]) -> BoxFuture<'_, ProviderResult>`.
- Produces unchanged `Executor::run(&self, req: &EffectRequest, ctx: &AttemptCtx) -> ExecOutcome`.
- Adds only private `deadline_remaining(deadline_ts: i64, at: SystemTime) -> Option<Duration>`; no interface changes for other tasks.

- [ ] **Step 1: Install the confirmed regression and run it red.**

```sh
cp docs/reviews/model-deadline-regression.rs.txt crates/agentos-engine/tests/review_deadline_probe.rs
docker compose run --rm test cargo test -p agentos-engine --test review_deadline_probe --locked -- --nocapture
```

Expected current failure: elapsed about three seconds and effect `Success`, despite deadline one second away. This exact probe failed during the repository review.

- [ ] **Step 2: Extend the regression's assertions.**

After its existing `assert!(outcome.unresolved, ...)`, assert that no receipt was retained:

```rust
assert_eq!(executor.retained_outcome(&request.effect_id), None);
```

Preserve the two-second outer assertion with a three-second provider: the failure is behavioral, not a scheduler timing margin of a few milliseconds.

- [ ] **Step 3: Add precise remaining-duration tests to `model/executor.rs`.**

```rust
#[cfg(test)]
mod deadline_tests {
    use super::*;

    #[test]
    fn remaining_duration_preserves_fractional_seconds() {
        let at = UNIX_EPOCH + Duration::from_millis(9_750);
        assert_eq!(deadline_remaining(10, at), Some(Duration::from_millis(250)));
    }

    #[test]
    fn elapsed_or_invalid_deadlines_have_no_remaining_duration() {
        let at = UNIX_EPOCH + Duration::from_secs(10);
        assert_eq!(deadline_remaining(10, at), None);
        assert_eq!(deadline_remaining(9, at), None);
        assert_eq!(deadline_remaining(-1, at), None);
    }
}
```

```sh
docker compose run --rm test cargo test -p agentos-engine --lib deadline_tests --locked
```

Expected initial compile failure: helper is not defined.

- [ ] **Step 4: Add the helper and replace the provider await.**

Add `Duration` to the existing `std::time` import. Add the private helper:

```rust
fn deadline_remaining(deadline_ts: i64, at: SystemTime) -> Option<Duration> {
    let seconds = u64::try_from(deadline_ts).ok()?;
    UNIX_EPOCH
        .checked_add(Duration::from_secs(seconds))?
        .duration_since(at)
        .ok()
        .filter(|remaining| !remaining.is_zero())
}
```

Replace the current `now()` pre-check and `match provider.complete(&req.payload).await` setup with:

```rust
let result = if req.deadline_ts == 0 {
    provider.complete(&req.payload).await
} else {
    let Some(remaining) = deadline_remaining(req.deadline_ts, SystemTime::now()) else {
        return ExecOutcome::failure(req, ctx, "deadline exceeded");
    };
    match tokio::time::timeout(remaining, provider.complete(&req.payload)).await {
        Ok(result) => result,
        Err(_) => ProviderResult::Transport("task deadline exceeded during model call".into()),
    }
};
let out = match result {
    ProviderResult::Response(bytes, _usage) if well_formed(&bytes) => {
        ExecOutcome::success(req, ctx, bytes)
    }
    ProviderResult::Response(bytes, _usage) => {
        let excerpt = String::from_utf8_lossy(&bytes[..bytes.len().min(EXCERPT)]).into_owned();
        ExecOutcome::failure(req, ctx, format!("malformed model response: {}", guest_text(&excerpt)))
    }
    ProviderResult::Rejected { status, body } => {
        ExecOutcome::failure(req, ctx, format!("http {status}: {}", guest_text(&body)))
    }
    ProviderResult::Transport(why) => {
        ExecOutcome::unresolved(req, ctx, format!("transport failure: {}", guest_text(&why)))
    }
};
```

Remove the old private `now()` helper after verifying this file has no remaining callers. Keep `counts.record` and retention logic in their current positions: the timeout is counted as an executed call, becomes unresolved through the existing transport arm, and is not retained as an applicable receipt.

- [ ] **Step 5: Run the relevant deadline, accounting, and crash gates.**

```sh
docker compose run --rm test cargo test -p agentos-engine --lib deadline_tests --locked
docker compose run --rm test cargo test -p agentos-engine --locked --test review_deadline_probe --test model_executor --test recover_forfeit --test model_flow --test model_crash_matrix --test deadline
docker compose run --rm test cargo test --workspace --locked
docker compose run --rm -e AGENTOS_TEST_WORKER=firecracker-fake -e AGENTOS_TEST_JAIL=fake test cargo test --workspace --locked
```

Expected: deadline probe green, expired requests still make zero sends, existing unresolved requests remain uncertain, successful responses still retain/replay, default and fake-jail regression suites pass. A timeout does not cancel a provider-side charge; it only stops local waiting and prevents more work after the task deadline.

- [ ] **Step 6: Format changed Rust files, inspect the diff, and commit the fix.**

```sh
docker compose run --rm test rustfmt --edition 2024 crates/agentos-engine/src/model/executor.rs crates/agentos-engine/tests/review_deadline_probe.rs
git diff --check
git diff -- crates/agentos-engine/src/model/executor.rs
git add crates/agentos-engine/src/model/executor.rs crates/agentos-engine/tests/review_deadline_probe.rs
git commit -m "fix(engine): bound model requests by the task deadline"
```

Rerun the focused tests if formatting or follow-up corrections change code. Workspace-wide fmt and strict Clippy were already failing at the reviewed baseline; their cleanup belongs to a separate CI-baseline change, not an unbounded expansion of this fix.

## Self-review and handoff

The task covers the confirmed deadline regression, precise timestamp conversion, existing no-send behavior, unchanged response retention, and uncertain accounting. No capability, worker, contract, provider, or schema interface changes are introduced. Cancellation responsiveness, HTTP retry policy, body caps, endpoint binding, GC, and Wasm are separately prioritized in the repository roadmap.

The plan is ready for implementation; no runtime change or commit was made as part of the review. Native execution is sufficient for this single focused task.
