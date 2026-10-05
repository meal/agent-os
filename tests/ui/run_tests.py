"""Keep failure artifacts useful without persisting local launch credentials."""
import contextlib
import io
import sys
import unittest
from common import ROOT, sanitize_test_output

captured = io.StringIO()
with contextlib.redirect_stdout(captured), contextlib.redirect_stderr(captured):
    suite = unittest.defaultTestLoader.discover(str(ROOT / 'tests/ui'), pattern=sys.argv[1])
    result = unittest.TextTestRunner(stream=captured, verbosity=2).run(suite)
output = sanitize_test_output(captured.getvalue())
(ROOT / 'build/ui-evidence/browser-tests.log').write_text(output)
sys.stdout.write(output)
sys.exit(0 if result.wasSuccessful() else 1)
