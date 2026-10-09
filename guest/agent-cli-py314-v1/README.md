# Python 3.14 with the Claude Code CLI (protocol 2)

The `python-stdlib-py314-v2` recipe (source-built kernel, pinned Python 3.14.8 through its hook)
plus the Claude Code native binary, so an external coding-agent CLI can run as the task's agent
inside the guest. It is a new recipe: the three recipes it is derived from stay unchanged, and
their digests stay cited by the released evidence.

| File | What it pins |
|------|--------------|
| `kernel.lock`, `snapshot.lock`, `packages.txt` | the same as `python-stdlib-py314-v2` (kernel by sha256, Debian snapshot 20260901) |
| `agent-cli.lock` | Claude Code `2.1.295` (npm `latest` on 2026-10-09; `stable` was 2.1.286): the tarball URL, its sha256, npm's `dist.integrity` (sha512) and the extracted binary's sha256 |
| `hooks/customize.sh` | calls the py314-v2 hook, then installs the binary |
| `image.json.in` | `"protocol":2`, id `agent-cli-py314-v1`, the same interpreter and kernel provenance as py314-v2 |

The binary comes from `@anthropic-ai/claude-code-linux-x64` (glibc, x86_64). It is dynamically
linked against the image's own glibc, so no Node runtime is installed. The hook downloads the
tarball with `curl` (https only) into a scratch directory under `/var/tmp`, checks its sha256
before `tar` reads it, extracts only `package/claude`, checks that binary's sha256, installs it
as `/opt/agent-cli/claude` (`root:root`, mode `0755`), and runs `ld-linux --list` on it so a
missing library fails the build. Nothing else is added, and nothing time-dependent is written:
the squashfs step pins every mtime and owner, so two builds are byte-identical.

## Build

```sh
COMPOSE_PROJECT_NAME=agentos-acceptance docker compose run --rm kernel-builder sh scripts/build-kernel.sh build/kernels/out --verify
COMPOSE_PROJECT_NAME=agentos-acceptance docker compose run --rm test-kvm sh scripts/build-guest-image.sh guest/agent-cli-py314-v1 build/guest-images/agent-cli-py314-v1 --verify
```

`--verify` builds twice and fails unless the three image files are identical. The last step of the
script registers the image and prints its digest.

To move to a new CLI release: change `AGENTOS_CLAUDE_CODE_VERSION` and `..._URL` in
`agent-cli.lock`, take the new tarball's sha256 and `dist.integrity` from
`npm view @anthropic-ai/claude-code-linux-x64@<version> dist.tarball dist.integrity` (download the
tarball and hash it yourself), and take the binary's sha256 from `package/claude` in that
tarball. The engine's test `agent_cli_lock_is_well_formed_and_names_the_npm_tarball` checks the
shape. The hook refuses a lock that does not name the npm tarball of its own version.

## Protocol 2

The guest protocol is 2 (`GUEST_PROTOCOL` in `crates/agentos-core/src/guest.rs`). A v2
controller refuses a manifest that says `"protocol":1`. The three older recipes (their
`image.json.in` still say protocol 1) are therefore not usable by it: they are kept as v1-protocol
images only, and this recipe is the one the agent session uses.

## Running the CLI

The session runs `argv[0]` by its absolute path, so `/opt/agent-cli/claude` needs no `PATH` entry
and no link in `/usr/bin` (the check's `PATH` is `/usr/bin:/bin` and is used only for checks).
The controller's `argv` is:

```
["/opt/agent-cli/claude", "-p", "<goal>", "--permission-mode", "bypassPermissions",
 "--disallowedTools", "WebFetch", "WebSearch"]
```

`--disallowedTools` lists the network tools, which cannot work without a NIC in the VM
(`docs/superpowers/specs/2026-10-09-guest-agent-runner-traffic.md` lists the CLI's other tools,
such as `Agent` and `Workflow`, and the session handler decides which of them to disallow). The guest sets the environment itself (`ANTHROPIC_BASE_URL` to
the in-VM proxy, a placeholder `ANTHROPIC_API_KEY`, `DISABLE_AUTOUPDATER=1`,
`CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`, `DISABLE_TELEMETRY=1`, `HOME` and `TMPDIR` under
scratch). The exact argv and the list of disallowed tools are confirmed on the KVM tier (Task 9b),
and the first billed run (Task 10) decides whether more request fields must be removed.

## Limits

- **Subscription logins are unsupported.** The CLI runs only with the API key the host supplies
  through the model proxy; no login, OAuth or credential store is provided in the image.
- Real-VM behaviour (loopback bring-up, uid handover, process limits for the native binary) is
  not yet shown; the fake-guest tests cover the session logic only.
