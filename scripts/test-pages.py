"""Check Pages links and execute its offline model example in the Compose test image."""
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


if __name__ == "__main__":
    unittest.main()
