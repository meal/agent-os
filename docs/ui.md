# Local browser workbench

Run tasks and review their recorded contracts, patches and verification evidence:

```sh
cargo run --locked -p agentos-cli -- --home /path/to/agentos-home --profiles /path/to/profiles ui --port 8080
# Linux Docker host networking:
docker compose run --rm ui
```

The server binds **127.0.0.1 only**. Open the `launch_url` printed at startup; its fragment authenticates this local session and is removed before requests. Treat that launch link as a credential. Keep the terminal open. Browser assets are bundled locally. Restarting the server invalidates sessions, forms and downloads; open the new launch link.

Compose uses Linux host networking so the loopback listener is accessible from the host. It publishes no container ports and adds no capabilities. `/work` contains the repository mounted read-only; build caches remain writable and `ui-home` persists `/data`. Docker Desktop networking is outside this supported launch path. To use an existing home, explicitly override the `/data` volume with its directory in a Compose override; inspect the resolved mounts with `docker compose config` before launching. The default named volume is separate from any existing host home.

Choose **New task**, paste an existing task-contract JSON document or load a local JSON file, select a model spec (`anthropic:model-id` or `fake:/server/path/transcript.json`) and choose the worker. File loading fills the textarea. Repository, profile and transcript paths refer to the **server filesystem**, including container paths when using Compose. For a host repository, add an explicit read-only mount and put that container path in the contract. Registry/profile/image paths are displayed in the form; manage them through `agentos profile register` and `agentos image register` in the same home. The form does not configure credentials, endpoints or launch binaries.

**Create task** records a READY task, staged repository/profile inputs and their digests. It issues no capabilities and makes no model request. Review the full contract's permissions, editable paths, limits, sandbox choice and recorded digests, then choose **Approve and start**. Approval checks that the contract and staged inputs still match. Retrying the same live form returns its existing task; changing a used form's inputs requires a new form. Forms expire after 15 minutes and each session holds at most 256; pending submissions are retained until completion.

Host workers execute on the server without VM isolation. Firecracker workers require a registered image and configured launcher; the standard UI Compose service does not supply KVM access or extra privileges. Provider credentials must be configured separately at server startup using the existing CLI options/environment. No key is built into the image or supplied by default. CLI-created READY tasks without a recorded agent need an agent supplied through the CLI before starting.

One driver can run in a home at a time. The UI rejects concurrent runs and respects external CLI drivers; it has no run queue. Pause and cancel follow the engine's existing state rules. Cancellation first records intent and can remain pending while another driver owns the home. Use **Resume** for a paused task or **Recover** for an interrupted run. Terminal tasks with outstanding effects may still require reconciliation; UNKNOWN effects retain their uncertainty. Server startup never approves or resumes work automatically.

Closing a tab leaves an admitted run active. Normal Ctrl-C/SIGTERM stops new admission and waits for owned operations to finish. Forced termination preserves journaled recovery, and a supervisor may finish its dispatched effect while the UI is down. Restart with the same home, open the new launch link and explicitly recover. The CLI remains available for recovery and registry maintenance.

Patch and verification views read the journal without exporting or changing task state. A verified-result label requires SUCCEEDED, matching final/verified workspace digests and accepted evidence. Polling marks failed reads stale and keeps the last view. Task pages are bounded to 50 tasks and event pages to 100 rows; contract/form input is limited to 256 KiB, patch previews to 256 KiB and event/log previews to 64 KiB. Full result/export collection is limited to 128 MiB. Oversized legacy results remain accessible through the CLI's existing behavior.

**Export** creates an authorized tar attachment with the CLI bundle's patch/evidence bytes. The UI keeps at most eight archives for ten minutes, scoped to the current session/task. Every download rechecks current export authority; revocation blocks even a cached archive. An expired or evicted archive requires another export. Export does not apply a patch to the source repository. Four query operations, four auxiliary controls and four active streams bound concurrent server work.

The local boundary enforces the exact listener Host, same-origin POST Origin, per-session CSRF, an HttpOnly SameSite=Strict cookie, no-store responses and a self-only content policy. This is a single-owner local tool, not a remote service or multi-user authentication system.

## Verification

```sh
sh scripts/check.sh
docker compose build test-ui
docker compose run --rm test-ui sh scripts/test-ui.sh
docker compose run --rm -e AGENTOS_UI_TEST_WORKER=firecracker-fake -e AGENTOS_TEST_JAIL=fake test-ui sh scripts/test-ui.sh
```

Chromium acceptance uses disposable homes, pyenv Python and an isolated browser virtual environment. The browser container has networking disabled, no provider credentials and no KVM. Host and fake-worker acceptance prove UI/engine integration; they do not establish real-provider or real-KVM acceptance. Sanitized logs and desktop/narrow screenshots are written to `build/ui-evidence/`. Dependency versions and upstream HTMX licensing are recorded in the lockfiles and `crates/agentos-cli/assets/ui/`.
