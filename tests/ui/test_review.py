import unittest
from pathlib import Path
from playwright.sync_api import sync_playwright, expect
from common import BrowserFixture, ROOT

class ReviewBrowserTests(unittest.TestCase):
    def test_result_view_is_read_only_and_keyboard_reachable(self):
        with BrowserFixture() as fixture, sync_playwright() as pw:
            task = fixture.seed_succeeded()
            fixture.start()
            browser = pw.chromium.launch(args=['--no-sandbox'])
            page = browser.new_page(viewport={'width': 1280, 'height': 900})
            foreign = []
            page.route('**/*', lambda route: route.continue_() if route.request.url.startswith(fixture.url + '/') else (foreign.append(route.request.url), route.abort()))
            page.set_default_timeout(5000)
            page.goto(fixture.launch_url)
            page.get_by_role('link', name='fix the parser', exact=True).click()
            before = fixture.events(task)
            page.get_by_role('tab', name='Patch', exact=True).click()
            expect(page.locator('.diff')).to_contain_text('key.strip()')
            page.get_by_role('tab', name='Verification', exact=True).click()
            expect(page.get_by_text('Verified final workspace', exact=True)).to_be_visible()
            self.assertEqual(fixture.events(task), before)
            self.assertEqual(foreign, [])
            page.screenshot(path=str(ROOT / 'build/ui-evidence/review-desktop.png'), full_page=True)
            page.set_viewport_size({'width': 390, 'height': 844})
            page.get_by_role('tab', name='Patch', exact=True).focus()
            page.keyboard.press('ArrowRight')
            expect(page.get_by_role('tab', name='Verification', exact=True)).to_be_focused()
            page.screenshot(path=str(ROOT / 'build/ui-evidence/review-narrow.png'), full_page=True)
            browser.close()

    def test_errors_preserve_review_and_render_untrusted_goal_as_text(self):
        with BrowserFixture() as fixture, sync_playwright() as pw:
            fixture.contract['goal'] = '<script>window.evil = true</script>'
            fixture.contract_path.write_text(__import__('json').dumps(fixture.contract))
            task = fixture.seed_succeeded()
            fixture.start()
            browser = pw.chromium.launch(args=['--no-sandbox'])
            page = browser.new_page()
            page.set_default_timeout(5000)
            page.goto(fixture.launch_url)
            page.get_by_role('link', name=fixture.contract['goal'], exact=True).click()
            self.assertIsNone(page.evaluate('window.evil'))
            fixture.cli('revoke', task, '--capability', 'artifact.export')
            page.get_by_role('tab', name='Patch', exact=True).click()
            expect(page.locator('#request-error')).to_contain_text('Request failed')
            expect(page.get_by_role('heading', name='Recorded contract')).to_be_visible()
            browser.close()

    def test_polling_tracks_an_external_driver_and_preserves_stale_state(self):
        with BrowserFixture() as fixture, sync_playwright() as pw:
            fixture.slow_verification()
            task = fixture.seed_ready()
            fixture.start()
            browser = pw.chromium.launch(args=['--no-sandbox'])
            page = browser.new_page()
            page.set_default_timeout(5000)
            page.goto(fixture.launch_url)
            page.get_by_role('link', name='fix the parser', exact=True).click()
            child = fixture.drive_cli(task)
            try:
                expect(page.locator('#status .state')).to_have_text('VERIFYING', timeout=15000)
                page.route('**/status', lambda route: route.abort())
                expect(page.locator('#freshness')).to_contain_text('Stale', timeout=8000)
                expect(page.locator('#status .state')).to_have_text('VERIFYING')
                page.unroute('**/status')
                fixture.release.touch()
                child.wait(timeout=20)
                expect(page.locator('#status .state')).to_have_text('SUCCEEDED', timeout=10000)
                page.get_by_role('tab', name='Events', exact=True).click()
                expect(page.locator('.event-page')).to_be_visible()
                rows = page.locator('[data-seq]').evaluate_all('(els) => els.map(el => el.dataset.seq)')
                self.assertEqual(len(rows), len(set(rows)))
            finally:
                fixture.release.touch()
                if child.poll() is None:
                    child.kill()
                child.wait(timeout=10)
                browser.close()

    def test_integrity_and_size_errors_are_visible_without_replacing_contract(self):
        import sqlite3
        from contextlib import closing
        with BrowserFixture() as fixture, sync_playwright() as pw:
            task = fixture.seed_succeeded()
            manifest = fixture.cli('export', task, fixture.root / 'bundle')
            fixture.start()
            browser = pw.chromium.launch(args=['--no-sandbox'])
            page = browser.new_page()
            page.set_default_timeout(5000)
            page.goto(fixture.launch_url)
            page.get_by_role('link', name='fix the parser', exact=True).click()
            digest = manifest['verification_results'][0]['evidence_digest']
            (fixture.home / 'blobs/objects' / digest[:2] / digest[2:]).write_bytes(b'corrupt')
            before = fixture.events(task)
            page.get_by_role('tab', name='Patch', exact=True).click()
            expect(page.locator('#request-error')).to_contain_text('503')
            expect(page.locator('#request-error')).to_contain_text('integrity')
            expect(page.get_by_role('heading', name='Recorded contract')).to_be_visible()
            with closing(sqlite3.connect(fixture.home / 'agentos.db')) as db:
                original = db.execute('SELECT contract_json FROM tasks WHERE id=?', (task,)).fetchone()[0]
                db.execute('UPDATE tasks SET contract_json=? WHERE id=?', ('x' * 300000, task))
                db.commit()
            page.get_by_role('tab', name='Verification', exact=True).click()
            expect(page.locator('#request-error')).to_contain_text('413')
            expect(page.get_by_role('heading', name='Recorded contract')).to_be_visible()
            with closing(sqlite3.connect(fixture.home / 'agentos.db')) as db:
                db.execute('UPDATE tasks SET contract_json=? WHERE id=?', (original, task))
                db.commit()
            self.assertEqual(fixture.events(task), before)
            browser.close()
