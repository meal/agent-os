import hashlib
import json
import tarfile
import unittest
from playwright.sync_api import sync_playwright, expect
from common import BrowserFixture, ROOT, create_in_browser

def source_digest(repo):
    digest = hashlib.sha256()
    for path in sorted(repo.rglob('*')):
        if path.is_file():
            digest.update(str(path.relative_to(repo)).encode())
            digest.update(path.read_bytes())
    return digest.hexdigest()

class WorkflowBrowserTests(unittest.TestCase):
    def test_create_approve_review_and_download_matches_cli(self):
        with BrowserFixture() as fixture, sync_playwright() as pw:
            original = source_digest(fixture.repo)
            fixture.start()
            browser = pw.chromium.launch(args=['--no-sandbox'])
            page = browser.new_page(viewport={'width': 1280, 'height': 900}, accept_downloads=True)
            page.set_default_timeout(10000)
            page.goto(fixture.launch_url)
            task = create_in_browser(page, fixture)
            self.assertEqual(fixture.status(task)['state'], 'READY')
            self.assertFalse(any(e['type'] == 'CapabilitiesIssued' for e in fixture.events(task)))
            expect(page.get_by_text('Approval applies to this recorded contract and staged inputs.')).to_be_visible()
            page.get_by_role('button', name='Approve and start', exact=True).click()
            fixture.wait_state(task, 'SUCCEEDED')
            expect(page.locator('#status .state')).to_have_text('SUCCEEDED', timeout=15000)
            page.get_by_role('tab', name='Patch', exact=True).click()
            expect(page.locator('.diff')).to_contain_text('key.strip()')
            page.get_by_role('tab', name='Verification', exact=True).click()
            expect(page.get_by_text('Verified final workspace', exact=True)).to_be_visible()
            with page.expect_download() as download:
                page.get_by_role('button', name='Export', exact=True).click()
            archive_path = fixture.root / 'result.tar'
            download.value.save_as(archive_path)
            extracted = fixture.root / 'extracted'
            with tarfile.open(archive_path) as archive:
                self.assertTrue(all(member.isfile() for member in archive.getmembers()))
                archive.extractall(extracted, filter='data')
            bundle = fixture.root / 'cli-bundle'
            fixture.cli('export', task, bundle)
            actual = {str(p.relative_to(extracted)): p.read_bytes() for p in extracted.rglob('*') if p.is_file()}
            expected = {str(p.relative_to(bundle)): p.read_bytes() for p in bundle.rglob('*') if p.is_file()}
            self.assertEqual(actual.keys(), expected.keys())
            for name in actual:
                if name == 'manifest.json':
                    a, b = json.loads(actual[name]), json.loads(expected[name])
                    a.pop('generated_events', None); b.pop('generated_events', None)
                    self.assertEqual(a, b)
                else: self.assertEqual(actual[name], expected[name], name)
            self.assertEqual(source_digest(fixture.repo), original)
            path = page.get_by_role('link', name='Download export archive', exact=True).get_attribute('href')
            fixture.cli('revoke', task, '--capability', 'artifact.export')
            before = fixture.events(task)
            response = page.request.get(fixture.url + path)
            self.assertEqual(response.status, 403)
            self.assertEqual(fixture.events(task), before)
            page.screenshot(path=str(ROOT / 'build/ui-evidence/workflow-desktop.png'), full_page=True)
            page.set_viewport_size({'width': 390, 'height': 844})
            page.get_by_role('tab', name='Patch', exact=True).focus()
            page.keyboard.press('ArrowRight')
            expect(page.get_by_role('tab', name='Verification', exact=True)).to_be_focused()
            page.screenshot(path=str(ROOT / 'build/ui-evidence/workflow-narrow.png'), full_page=True)
            browser.close()

    def test_missing_key_and_outdated_approval_preserve_ready(self):
        with BrowserFixture() as fixture, sync_playwright() as pw:
            fixture.start()
            browser = pw.chromium.launch(args=['--no-sandbox'])
            page = browser.new_page()
            page.set_default_timeout(10000)
            page.goto(fixture.launch_url)
            task = create_in_browser(page, fixture, 'anthropic:test-model')
            before = fixture.events(task)
            page.get_by_role('button', name='Approve and start', exact=True).click()
            expect(page.locator('#request-error')).to_contain_text('API key')
            self.assertEqual(fixture.status(task)['state'], 'READY')
            self.assertEqual(fixture.events(task), before)
            page.get_by_role('link', name='New task', exact=True).click()
            # The form is invalidated by a restart, including its cookie and nonce.
            nonce = page.locator('[name=nonce]').get_attribute('value')
            fixture.restart()
            response = page.request.get(fixture.url + '/tasks')
            self.assertEqual(response.status, 403)
            page.goto(fixture.launch_url)
            expect(page.get_by_role('heading', name='Tasks', exact=True)).to_be_visible()
            csrf = page.evaluate("document.cookie") # HttpOnly session remains inaccessible.
            self.assertNotIn('agentos_session', csrf)
            page.get_by_role('link', name='New task', exact=True).click()
            page.locator('[name=nonce]').evaluate('(el, nonce) => el.value = nonce', nonce)
            page.get_by_label('Task contract JSON').fill(fixture.create_contract_json())
            page.get_by_label('Model', exact=True).fill('fake:/work/fixtures/transcripts/parser-fix.json')
            page.get_by_role('button', name='Create task', exact=True).click()
            expect(page.locator('#request-error')).to_contain_text('410')
            browser.close()

    def test_ordinary_forms_and_local_json_file_remain_usable_without_htmx(self):
        with BrowserFixture() as fixture, sync_playwright() as pw:
            fixture.start()
            browser = pw.chromium.launch(args=['--no-sandbox'])
            context = browser.new_context()
            page = context.new_page()
            page.goto(fixture.launch_url)
            page.get_by_role('link', name='New task', exact=True).click()
            page.get_by_label('Load a local JSON file').set_input_files(fixture.contract_path)
            expect(page.get_by_label('Task contract JSON')).to_have_value(fixture.create_contract_json())
            offline = browser.new_context(java_script_enabled=False)
            offline.add_cookies(context.cookies())
            native = offline.new_page()
            native.goto(fixture.url + '/tasks/new')
            native.get_by_label('Task contract JSON').fill(fixture.create_contract_json())
            native.get_by_label('Model', exact=True).fill('fake:/work/fixtures/transcripts/parser-fix.json')
            native.get_by_label('Worker', exact=True).select_option('firecracker' if fixture.worker == 'firecracker-fake' else 'host')
            origins = []
            native.on('request', lambda request: origins.append(request.headers.get('origin')) if request.method == 'POST' else None)
            native.get_by_role('button', name='Create task', exact=True).click()
            self.assertFalse(native.locator('.error').count(), native.locator('body').inner_text())
            self.assertEqual(origins[0], fixture.url)
            task = native.locator('[data-task-id]').get_attribute('data-task-id')
            native.get_by_role('button', name='Approve and start', exact=True).click()
            expect(native.get_by_role('heading', name='fix the parser', exact=True)).to_be_visible()
            fixture.wait_state(task, 'SUCCEEDED')
            native.goto(fixture.url + '/tasks/' + task)
            native.get_by_role('button', name='Export', exact=True).click()
            expect(native.get_by_role('link', name='Download export archive')).to_be_visible()
            browser.close()

    def test_outdated_action_form_and_changed_profile_refuse_before_grants(self):
        with BrowserFixture() as fixture, sync_playwright() as pw:
            fixture.start()
            browser = pw.chromium.launch(args=['--no-sandbox'])
            page = browser.new_page()
            page.goto(fixture.launch_url)
            task = create_in_browser(page, fixture)
            before = fixture.events(task)
            form = page.locator('form[action$="/start"]')
            digest = form.locator('[name=contract_digest]').get_attribute('value')
            form.locator('[name=contract_digest]').evaluate('(el) => el.value = "0".repeat(64)')
            page.get_by_role('button', name='Approve and start', exact=True).click()
            expect(page.locator('#request-error')).to_contain_text('409')
            self.assertEqual(fixture.events(task), before)
            form.locator('[name=contract_digest]').evaluate('(el, value) => el.value = value', digest)
            (fixture.home / 'tasks' / task / 'profile/tampered.txt').write_text('changed')
            page.get_by_role('button', name='Approve and start', exact=True).click()
            expect(page.locator('#request-error')).to_contain_text('profile')
            self.assertEqual(fixture.status(task)['state'], 'READY')
            self.assertEqual(fixture.events(task), before)
            browser.close()
