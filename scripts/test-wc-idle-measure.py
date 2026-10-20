#!/usr/bin/env python3
"""Local failure and lifecycle checks; no guest or host access."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('measure', Path(__file__).with_name('wc-idle-measure.py'))
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)


def config():
    c = json.loads(Path(__file__).with_name('wc-idle-measure.example.json').read_text())
    c.update(guest_sha256='a' * 64, app_sha256='b' * 64, source_commit='c' * 40)
    c['host_binaries'] = {p: 'd' * 64 for p in c['host_binaries']}
    return c


class Validation(unittest.TestCase):
    def test_admin_profile_and_shared_install_refused(self):
        for changes in ({'linux_user': 'admin'}, {'install': 'wcivs'},
                        {'app_exec': '/nix/store/patched/bin/firefox'}, {'test_home': '/home/wctest'}):
            with self.subTest(changes=changes), self.assertRaises(m.Refused):
                m.validate(config() | changes)

    def test_ambiguous_or_wrong_mapping_refused(self):
        c = config()
        valid = f"{c['server']} {c['guest_user']} wctest installs=wcil\n"
        m.check_mapping(valid, c)
        for invalid in ('', valid.replace('wctest', 'admin'), valid + valid):
            with self.subTest(invalid=invalid), self.assertRaises(m.Refused):
                m.check_mapping(invalid, c)

    def test_app_defaults_and_isolation(self):
        c = config()
        text = f"[foot]\nexec={c['app_exec']}\nenv=HOME={c['test_home']}/home\n"
        m.check_app(text, c)
        for bad in (text + 'args=--config=/private\n', text + 'env=HOME=/home/admin\n',
                    text.replace(c['app_exec'], '/bin/sh')):
            with self.assertRaises(m.Refused):
                m.check_app(bad, c)

    def test_isolated_trace_and_rejections(self):
        trace = '1 L.send\n200001 L.recv\n1000200001 L.send\n1000400001 L.recv\n'
        self.assertEqual(len(m.parse_samples(trace, 2)), 2)
        for bad in (trace.replace('1000200001', '300001'), trace + '2000000000 L.send\n',
                    trace.replace('1 L.send\n', '', 1), 'broken\n'):
            with self.subTest(bad=bad), self.assertRaises(m.Refused):
                m.parse_samples(bad, 2)
        with self.assertRaises(m.Refused):
            m.parse_samples(trace, 3)

    def test_aggregate_requires_2000_and_one_build(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            def put(tag, count, ns, commit='a'):
                (root / tag).mkdir()
                (root / tag / 'result.json').write_text(json.dumps({
                    'kind': 'batch', 'source_commit': commit, 'guest_sha256': 'b',
                    'count': count, 'samples_ns': [[i * 1001000000, i * 1001000000 + ns] for i in range(count)]}))
            put('one', 500, 100000)
            with self.assertRaises(m.Refused):
                m.report(root, 2000)
            put('two', 1500, 300000)
            result = m.report(root, 2000)
            self.assertEqual(result['mean_us'], 250)
            self.assertFalse(result['target_mean_below_200us'])
            put('other-build', 1, 1000, 'different')
            with self.assertRaises(m.Refused):
                m.report(root, 2000)

    def test_wrong_system_layout_never_touches_guest(self):
        harness = m.Harness(config())
        harness.command = lambda *a, **k: ''
        harness.host = lambda script: 'active\nadmin\n'
        with patch.object(harness, 'guest') as guest, self.assertRaises(m.Refused):
            harness.preflight()
        guest.assert_not_called()


class Stub(m.Harness):
    def __init__(self, c, fail_ping=False, fail_stop=False):
        super().__init__(c)
        self.actions = []
        self.fail_ping = fail_ping
        self.fail_stop = fail_stop
        self.checks = 0

    def command(self, argv, *args, **kwargs):
        self.actions.append(argv[1])
        return ''

    def preflight(self):
        self.checks += 1

    def launch(self, *args, **kwargs):
        self.actions.append('launch')

    def wait_log(self, *args, **kwargs):
        return '', ''

    def live_identity(self, *args, **kwargs):
        pass

    def guest(self, script, **kwargs):
        if script.startswith('Test-Path'):
            return 'False'
        if 'WC_PING_N' in script:
            if self.fail_ping:
                raise m.Refused('synthetic ping failure')
            return ''
        if script.startswith('Get-Content'):
            return '1 L.send\n200001 L.recv\n1000200001 L.send\n1000400001 L.recv\n'
        return ''

    def read_log(self):
        return 'transport rings: confirmed'

    def stop(self):
        self.actions.append('stop')
        if self.fail_stop:
            raise m.Refused('synthetic stop failure')

    def health(self):
        self.actions.append('health')


class Lifecycle(unittest.TestCase):
    def test_insufficient_lease_fails_before_remote_access(self):
        harness = Stub(config())
        with tempfile.TemporaryDirectory() as directory, self.assertRaises(m.Refused):
            harness.run('batch', 500, 5, Path(directory) / 'bad-budget')
        self.assertEqual(harness.actions, [])

    def test_health_failure_does_not_issue_guest_cleanup(self):
        harness = Stub(config(), fail_ping=True)
        original = harness.guest
        def fail(script, **kwargs):
            if 'WC_PING_N' in script:
                harness.failed.set()
            return original(script, **kwargs)
        harness.guest = fail
        with tempfile.TemporaryDirectory() as directory:
            out = Path(directory) / 'unhealthy'
            with self.assertRaises(m.Refused):
                harness.run('batch', 2, 12, out)
            self.assertFalse((out / 'result.json').exists())
        self.assertNotIn('stop', harness.actions)
        self.assertEqual(harness.actions[-1], 'release')

    def run_stub(self, fail_ping=False, fail_stop=False):
        c = config()
        h = Stub(c, fail_ping, fail_stop)
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        out = Path(tmp.name) / 'batch'
        # Advance the local wall clock; guest timing remains in the fixture.
        clock = iter(range(0, 1000, 10))
        with patch.object(m.time, 'monotonic', side_effect=lambda: next(clock)):
            if fail_ping or fail_stop:
                with self.assertRaises(m.Refused):
                    h.run('batch', 2, 12, out)
            else:
                h.run('batch', 2, 12, out)
        self.assertEqual(h.checks, 2)
        self.assertEqual(h.actions[-1], 'release')
        self.assertEqual('stop' in h.actions, True)
        self.assertEqual((out / 'result.json').exists(), not (fail_ping or fail_stop))
        return h

    def test_success_has_two_preflights_and_scoped_cleanup(self):
        self.run_stub()

    def test_failed_measurement_cleans_up_without_manifest(self):
        self.run_stub(fail_ping=True)

    def test_failed_cleanup_is_not_a_completed_batch(self):
        self.run_stub(fail_stop=True)


if __name__ == '__main__':
    unittest.main()
