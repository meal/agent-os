import unittest
from common import sanitize_test_output, SECRETS

class EvidenceTests(unittest.TestCase):
    def test_browser_failure_evidence_redacts_session_secrets_and_launch_fragments(self):
        SECRETS.add('owned-token-sentinel')
        output = sanitize_test_output('goto http://127.0.0.1:8080/#unregistered-secret\nowned-token-sentinel\nGET http://127.0.0.1:8080/tasks')
        self.assertNotIn('unregistered-secret', output)
        self.assertNotIn('owned-token-sentinel', output)
        self.assertIn('/tasks', output)
