import unittest
from collections import Counter
from playwright.sync_api import sync_playwright, expect
from common import BrowserFixture, create_in_browser

class RecoveryBrowserTests(unittest.TestCase):
    def test_closing_tab_leaves_owned_run_active(self):
        with BrowserFixture() as fixture, sync_playwright() as pw:
            fixture.slow_profile()
            fixture.start()
            browser = pw.chromium.launch(args=['--no-sandbox'])
            page = browser.new_page()
            page.goto(fixture.launch_url)
            task = create_in_browser(page, fixture)
            page.get_by_role('button', name='Approve and start', exact=True).click()
            fixture.wait_entered()
            page.close()
            self.assertEqual(fixture.status(task)['state'], 'VERIFYING')
            fixture.release.touch()
            fixture.wait_state(task, 'SUCCEEDED')
            browser.close()

    def test_sigkill_restart_requires_explicit_recovery_and_preserves_completed_effects(self):
        with BrowserFixture() as fixture, sync_playwright() as pw:
            fixture.slow_profile()
            fixture.start()
            browser = pw.chromium.launch(args=['--no-sandbox'])
            page = browser.new_page()
            page.set_default_timeout(10000)
            page.goto(fixture.launch_url)
            task = create_in_browser(page, fixture)
            page.get_by_role('button', name='Approve and start', exact=True).click()
            fixture.wait_entered()
            before = fixture.events(task)
            completed = [e['payload']['effect_id'] for e in before if e['type'] == 'EffectCompleted']
            self.assertTrue(completed)
            self.assertTrue(any(e['state'] == 'Dispatched' for e in fixture.status(task)['outstanding_effects']))
            fixture.restart() # Kills only this fixture's UI controller, not its supervisor.
            page.goto(fixture.launch_url)
            page.get_by_role('link', name='fix the parser', exact=True).click()
            expect(page.get_by_role('button', name='Recover', exact=True)).to_be_enabled()
            self.assertEqual(fixture.events(task), before)
            self.assertEqual(fixture.status(task)['state'], 'VERIFYING')
            page.get_by_role('button', name='Recover', exact=True).click()
            fixture.release.touch()
            fixture.wait_state(task, 'SUCCEEDED')
            counts = Counter(e['payload']['effect_id'] for e in fixture.events(task) if e['type'] == 'EffectCompleted')
            self.assertTrue(all(counts[effect] == 1 for effect in completed))
            self.assertTrue(all(count == 1 for count in counts.values()))
            browser.close()

    def test_browser_and_cli_pause_resume_and_browser_cancel(self):
        with BrowserFixture() as fixture, sync_playwright() as pw:
            first = fixture.crash_after_patch_dispatch()
            fixture.start()
            browser = pw.chromium.launch(args=['--no-sandbox'])
            page = browser.new_page()
            page.set_default_timeout(10000)
            page.goto(fixture.launch_url)
            page.goto(fixture.url + '/tasks/' + first)
            page.get_by_role('button', name='Pause', exact=True).click()
            expect(page.locator('#status .state')).to_have_text('PAUSED')
            fixture.cli('resume', first)
            fixture.wait_state(first, 'SUCCEEDED')
            second = fixture.crash_after_patch_dispatch()
            fixture.cli('pause', second)
            page.goto(fixture.url + '/tasks/' + second)
            page.get_by_role('button', name='Resume', exact=True).click()
            fixture.wait_state(second, 'SUCCEEDED')
            fixture.slow_profile()
            # The server uses its configured profiles; restart selects the owned slow registry.
            fixture.restart()
            page.goto(fixture.launch_url)
            third = create_in_browser(page, fixture)
            page.get_by_role('button', name='Approve and start', exact=True).click()
            fixture.wait_entered()
            expect(page.get_by_role('button', name='Cancel', exact=True)).to_be_visible()
            page.get_by_role('button', name='Cancel', exact=True).click()
            fixture.wait_state(third, 'CANCELLED')
            browser.close()
