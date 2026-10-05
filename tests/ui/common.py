"""Owned offline fixtures: no lifecycle SQL, external API, or shared child processes."""
import json
import os
from pathlib import Path
import selectors
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[2]
BINARY = ROOT / 'target/debug/agentos'
SECRETS = set()

def sanitize_test_output(output):
    import re
    for secret in SECRETS:
        output = output.replace(secret, '[redacted]')
    return re.sub(r"(http://127\.0\.0\.1:\d+/)#([^\s\"']+)", r'\1#[redacted]', output)

SCRUBBED = ['ANTHROPIC_API_KEY', 'AGENTOS_API_KEY_FILE', 'AGENTOS_ANTHROPIC_BASE_URL',
            'AGENTOS_WORKER', 'AGENTOS_FIRECRACKER', 'AGENTOS_JAILER', 'AGENTOS_JAIL_UID',
            'AGENTOS_JAIL_GID', 'AGENTOS_ALLOW_UNJAILED', 'AGENTOS_TEST_WORKERS',
            'AGENTOS_TEST_FAKE_GUEST', 'AGENTOS_TEST_JAIL_PROBE', 'AGENTOS_GUEST_IMAGE']

class BrowserFixture:
    def __init__(self):
        self.temp = tempfile.TemporaryDirectory(prefix='agentos-browser-')
        self.root = Path(self.temp.name)
        self.home = self.root / 'home'
        self.repo = self.root / 'repo'
        shutil.copytree(ROOT / 'fixtures/parser-repo', self.repo)
        self.contract_path = self.root / 'task.json'
        self.contract = {'goal': 'fix the parser', 'repository': {'source': str(self.repo), 'revision': 'recorded-at-submission'},
            'profile': 'python-stdlib-v1', 'verification_profile': 'parser-checks-v1', 'editable_paths': ['src/**'],
            'capabilities': ['snapshot.read', 'workspace.apply_patch', 'verification.run', 'artifact.export', 'model.request'],
            'limits': {'model_requests': 12, 'max_output_tokens_per_request': 4096, 'tool_actions': 50,
                       'deadline_seconds': 600, 'worker_vcpus': 1, 'worker_memory_mib': 256}}
        self.contract_path.write_text(json.dumps(self.contract))
        self.worker = os.environ.get('AGENTOS_UI_TEST_WORKER', 'host')
        if self.worker not in ('host', 'firecracker-fake'):
            raise ValueError('AGENTOS_UI_TEST_WORKER must be host or firecracker-fake')
        self.process = None
        self.environment = {k: v for k, v in os.environ.items() if k not in SCRUBBED}
        self.flags = []
        self.profiles = ROOT / 'fixtures/profiles'
        if self.worker == 'firecracker-fake':
            self.environment.update(AGENTOS_TEST_WORKERS='1', AGENTOS_TEST_FAKE_GUEST='1', AGENTOS_TEST_JAIL_PROBE='ok')
            self.flags = ['--worker', 'firecracker']
            self.register_image()
    def __enter__(self):
        return self
    def __exit__(self, *_):
        self.stop()
        self.temp.cleanup()
    def command(self, *args):
        return [str(BINARY), '--home', str(self.home), '--profiles', str(self.profiles), *self.flags, *map(str, args)]
    def cli(self, *args):
        result = subprocess.run(self.command(*args), env=self.environment, capture_output=True, text=True, timeout=45)
        if result.returncode: raise AssertionError(result.stderr)
        return json.loads(result.stdout)
    def events(self, task):
        result = subprocess.run(self.command('events', task), env=self.environment, capture_output=True, text=True, timeout=10, check=True)
        return [json.loads(line) for line in result.stdout.splitlines()]
    def status(self, task):
        return self.cli('status', task)
    def seed_succeeded(self):
        result = self.cli('submit', self.contract_path, '--fake-agent-patch', ROOT / 'fixtures/parser-repo.fix.patch', '--yes')
        assert result['state'] == 'SUCCEEDED', result
        return result['task_id']
    def start(self):
        self.errors = open(self.root / 'server.stderr', 'a+')
        self.process = subprocess.Popen(self.command('ui', '--port', '0'), env=self.environment,
                                        stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=self.errors, text=True)
        with selectors.DefaultSelector() as ready:
            ready.register(self.process.stdout, selectors.EVENT_READ)
            if not ready.select(15):
                raise RuntimeError('UI startup timed out')
            line = self.process.stdout.readline()
        info = json.loads(line)
        self.url, self.launch_url = info['listening'], info['launch_url']
        SECRETS.add(self.launch_url.split('#', 1)[1])
    def stop(self):
        if self.process is not None:
            if self.process.poll() is None:
                self.process.kill()
            self.process.wait(timeout=10)
            self.process.stdout.close()
            self.process = None
            self.errors.close()
    def register_image(self):
        image = self.root / 'image'
        image.mkdir()
        (image / 'vmlinux').write_bytes(bytes([0x7f]) * 16)
        (image / 'rootfs.squashfs').write_bytes(bytes([0x68]) * 16)
        (image / 'image.json').write_text(json.dumps({'id': 'python-stdlib-v1', 'protocol': 1,
            'kernel': 'vmlinux', 'rootfs': 'rootfs.squashfs', 'agent_version': '0.1.0',
            'kernel_sha256': '0545ba1781fc06cfa1d7699069057f4538103fd1644100cf0da434899a1ed447',
            'built_from': 'test'}))
        self.cli('image', 'register', image)

    def slow_verification(self):
        self.profiles = self.root / 'profiles'
        shutil.copytree(ROOT / 'fixtures/profiles', self.profiles)
        self.entered = self.root / 'verification-entered'
        self.release = self.root / 'verification-release'
        path = self.profiles / 'parser-checks-v1/check_parser.py'
        gate = "import pathlib, time\npathlib.Path(%r).write_text('entered')\nwhile not pathlib.Path(%r).exists(): time.sleep(0.02)\n" % (str(self.entered), str(self.release))
        original = path.read_text()
        # Keep any future import at the beginning of the original check script.
        path.write_text(original.replace('import ', gate + 'import ', 1))
    def seed_ready(self):
        return self.cli('submit', self.contract_path, '--fake-agent-patch', ROOT / 'fixtures/parser-repo.fix.patch')['task_id']
    def drive_cli(self, task):
        output = open(self.root / 'driver.output', 'w')
        child = subprocess.Popen(self.command('resume', task), env=self.environment, stdout=output, stderr=output)
        output.close()
        return child

    def create_contract_json(self):
        return json.dumps(self.contract)
    def wait_state(self, task, state, timeout=60):
        import time
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            if self.status(task)['state'] == state: return
            time.sleep(0.02)
        raise AssertionError('Task did not reach ' + state)
    def wait_entered(self, timeout=20):
        import time
        end = time.monotonic() + timeout
        while not self.entered.exists():
            if time.monotonic() >= end: raise AssertionError('Verification did not enter owned barrier')
            time.sleep(0.02)
    def slow_profile(self):
        self.slow_verification()
    def restart(self):
        self.stop()
        self.start()
    def crash_after_patch_dispatch(self):
        result = subprocess.run(self.command('submit', self.contract_path, '--fake-agent-patch', ROOT / 'fixtures/parser-repo.fix.patch', '--yes', '--crash-at', 'after-dispatch:apply_patch'), env=self.environment, capture_output=True, text=True, timeout=20)
        assert result.returncode == 75, result.stderr
        for line in result.stderr.splitlines():
            if line.startswith('{'):
                record = json.loads(line)
                if 'crashed' in record: return record['task_id']
        raise AssertionError('Missing injected crash record')

def create_in_browser(page, fixture, model=None):
    from playwright.sync_api import expect
    page.get_by_role('link', name='New task', exact=True).click()
    page.get_by_label('Task contract JSON').fill(fixture.create_contract_json())
    page.get_by_label('Model', exact=True).fill(model or 'fake:/work/fixtures/transcripts/parser-fix.json')
    page.get_by_label('Worker', exact=True).select_option('firecracker' if fixture.worker == 'firecracker-fake' else 'host')
    page.get_by_role('button', name='Create task', exact=True).click()
    expect(page.locator('[data-task-id]')).to_be_visible()
    return page.locator('[data-task-id]').get_attribute('data-task-id')
