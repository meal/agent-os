"""Check Pages links and execute its offline usage examples in the Compose test image."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from html.parser import HTMLParser

REPO = Path(__file__).resolve().parent.parent
PAGE = REPO / "docs/index.html"


class Walkthrough(HTMLParser):
    def __init__(self):
        super().__init__()
        self.ids = []
        self.links = []
        self.examples = {}
        self.current = None

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if "id" in attrs:
            self.ids.append(attrs["id"])
        if tag == "a" and "href" in attrs:
            self.links.append(attrs["href"])
        if tag == "pre" and "data-demo" in attrs:
            self.current = attrs["data-demo"]
            self.examples[self.current] = ""

    def handle_endtag(self, tag):
        if tag == "pre":
            self.current = None

    def handle_data(self, data):
        if self.current:
            self.examples[self.current] += data


class PagesTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.page = Walkthrough()
        cls.page.feed(PAGE.read_text())

    def test_local_links_and_fragment_targets_resolve(self):
        self.assertEqual(len(self.page.ids), len(set(self.page.ids)))
        self.assertTrue(self.page.links, "walkthrough needs documentation links")
        for link in self.page.links:
            github_prefix = "https://github.com/meal/agent-os/blob/main/"
            if link.startswith(github_prefix):
                self.assertTrue((REPO / link.removeprefix(github_prefix)).is_file(), f"missing repository doc: {link}")
                continue
            if ":" in link or link.startswith("//"):
                continue
            filename, _, fragment = link.partition("#")
            target = PAGE.parent / filename if filename else PAGE
            self.assertTrue(target.is_file(), f"missing local link: {link}")
            if fragment and target.resolve() == PAGE.resolve():
                self.assertIn(fragment, self.page.ids, f"missing fragment: {link}")

    def test_offline_model_example_repairs_and_exports_the_fixture(self):
        contract = self.page.examples["model-contract"]
        json.loads(contract)  # Diagnose broken example JSON before invoking the CLI.
        commands = self.page.examples["model-commands"]
        with tempfile.TemporaryDirectory(prefix="agentos-pages-") as scratch:
            root = Path(scratch)
            task = root / "model-task.json"
            task.write_text(contract)
            result = subprocess.run(
                ["sh", "-eu", "-c", commands],
                cwd=REPO,
                env={**os.environ, "DEMO_CONTRACT": str(task), "DEMO_HOME": str(root / "home")},
                capture_output=True,
                text=True,
                timeout=60,
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            manifest = json.loads((root / "home/bundle/manifest.json").read_text())
            self.assertEqual(manifest["state"], "SUCCEEDED")
            self.assertEqual(manifest["model"], "fake:parser-fix.json")
            self.assertEqual(len(manifest["model_calls"]), 6)
            self.assertEqual(manifest["final_workspace_digest"], manifest["verified_digest"])
            self.assertEqual(manifest["model_policy_version"], 1)
            self.assertEqual(manifest["model_limits_version"], 1)
            self.assertTrue((root / "home/bundle/patch.diff").is_file())

    def test_real_world_example_requires_approval_and_reproduces_the_export(self):
        self.assertIn("real-world-commands", self.page.examples, "Pages needs a runnable review-and-apply example")
        self.assertIn("real-world-review", self.page.examples, "Approval must be a separate step")
        with tempfile.TemporaryDirectory(prefix="agentos-practice-") as scratch:
            root = Path(scratch)
            contract = root / "model-task.json"
            contract.write_text(self.page.examples["model-contract"])
            practice = root / "practice"
            result = subprocess.run(
                ["sh", "-eu", "-c", self.page.examples["real-world-commands"] + "\n" + self.page.examples["real-world-review"]],
                cwd=REPO,
                env={**os.environ, "MODEL_CONTRACT": str(contract), "PRACTICE_DIR": str(practice)},
                capture_output=True,
                text=True,
                timeout=60,
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertEqual(json.loads((practice / "approval.json").read_text())["state"], "READY")
            ready = json.loads((practice / "ready-status.json").read_text())
            self.assertEqual(ready["state"], "READY")
            self.assertEqual(ready["actions_used"], 0)
            self.assertEqual(ready["usage"]["settled_model_requests"], 0)
            self.assertEqual(ready["outstanding_effects"], [])
            manifest = json.loads((practice / "bundle/manifest.json").read_text())
            self.assertEqual(manifest["state"], "SUCCEEDED")
            self.assertEqual(manifest["final_workspace_digest"], manifest["verified_digest"])
            self.assertTrue(manifest["verification_results"][-1]["accepted_for_final_workspace"])
            original = (REPO / "fixtures/parser-repo/src/parser.py").read_bytes()
            self.assertEqual((practice / "service/src/parser.py").read_bytes(), original)
            self.assertNotEqual((practice / "review/src/parser.py").read_bytes(), original)
            for relative in ("tests/test_parser.py", "tests/__init__.py", "src/__init__.py"):
                self.assertEqual((practice / "service" / relative).read_bytes(), (practice / "review" / relative).read_bytes())
            self.assertIn("10/10 checks passed", result.stdout)


if __name__ == "__main__":
    unittest.main()
