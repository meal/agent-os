"""Summarizes an alpha evidence run (scripts/alpha-evidence.sh) into summary.json and summary.md.

    python3 scripts/alpha-summary.py build/evidence/alpha-<time>-<pid> OUT_DIR

For each snapshot and agent: the final state, model requests settled and uncertain, tool
actions, the contract limits, the duration, and the digests of the exported patch, the
verified workspace, the verification evidence and the analysis report, with the versions from
the release manifest.
"""

import json
import sys
from pathlib import Path


def load(path):
    try:
        return json.loads(Path(path).read_text())
    except (OSError, ValueError):
        return None


def run_summary(run_dir, contract):
    status = load(run_dir / "status.json") or {}
    manifest = load(run_dir / "manifest.json") or {}
    timing = load(run_dir / "run.json") or {}
    usage = status.get("usage", {})
    accepted = [r for r in manifest.get("verification_results", []) if r.get("accepted_for_final_workspace")]
    analysis = manifest.get("analysis") or {}
    return {
        "task_id": timing.get("task_id"),
        "state": status.get("state"),
        "seconds": timing.get("seconds"),
        "model_requests_settled": usage.get("settled_model_requests"),
        "model_requests_uncertain": usage.get("uncertain_model_requests"),
        "tool_actions_settled": usage.get("settled_tool_actions"),
        "limits": contract.get("limits"),
        "jailed": status.get("jailed"),
        "patch_digest": manifest.get("patch_digest"),
        "verified_digest": manifest.get("verified_digest"),
        "final_workspace_digest": manifest.get("final_workspace_digest"),
        "verification_evidence_digest": accepted[0]["evidence_digest"] if accepted else None,
        "verification_profile_digest": manifest.get("verification_profile_digest"),
        "analysis_state": analysis.get("state"),
        "analysis_report_digest": analysis.get("report_digest"),
        "model": manifest.get("model"),
        "model_calls": len(manifest.get("model_calls", [])),
    }


def main():
    src, out = Path(sys.argv[1]), Path(sys.argv[2])
    out.mkdir(parents=True, exist_ok=True)
    release = load(src / "release-MANIFEST.json") or {}
    contracts = {
        "fake": {"limits": {"model_requests": 1, "max_output_tokens_per_request": 1000,
                            "tool_actions": 10, "deadline_seconds": 600}},
        "live": {"limits": {"model_requests": 12, "max_output_tokens_per_request": 16000,
                            "tool_actions": 12, "deadline_seconds": 900}},
    }
    runs = []
    for snapshot in ["parser", "duration"]:
        for agent in ["fake", "live"]:
            run_dir = src / f"{snapshot}-{agent}"
            if not run_dir.is_dir():
                runs.append({"snapshot": snapshot, "agent": agent, "state": "NOT RUN"})
                continue
            entry = {"snapshot": snapshot,
                     "agent": "deterministic (killed and resumed)" if agent == "fake" else "live model"}
            entry.update(run_summary(run_dir, contracts[agent]))
            runs.append(entry)
    summary = {"schema_version": 1, "release": release.get("version"), "commit": release.get("commit"),
               "versions": release.get("versions"), "image": release.get("image"),
               "profiles": release.get("profiles"), "component": release.get("component"), "runs": runs}
    (out / "summary.json").write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n")
    lines = ["| Snapshot | Agent | State | Model requests (settled / uncertain) | Tool actions | Seconds | Patch | Verified workspace | Analysis |",
             "| --- | --- | --- | --- | --- | --- | --- | --- | --- |"]
    short = lambda d: f"`{d[:12]}`" if d else "none"
    for r in runs:
        lines.append(
            f"| {r['snapshot']} | {r['agent']} | {r.get('state')} | "
            f"{r.get('model_requests_settled')} / {r.get('model_requests_uncertain')} | "
            f"{r.get('tool_actions_settled')} | {r.get('seconds')} | {short(r.get('patch_digest'))} | "
            f"{short(r.get('verified_digest'))} | {r.get('analysis_state')} |")
    (out / "summary.md").write_text("\n".join(lines) + "\n")
    print("\n".join(lines))


if __name__ == "__main__":
    main()
