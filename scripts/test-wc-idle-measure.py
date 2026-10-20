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
    c.update(guest_sha256='a' * 64, compositor_sha256='e' * 64, app_sha256='b' * 64, source_commit='c' * 40)
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
            def put(tag, count, ns, commit='a', host_hash='d'):
                (root / tag).mkdir()
                manifest = {'schema': 1, 'source_commit': commit, 'host_binaries': {'wc-proxy': host_hash}}
                (root / tag / 'result.json').write_text(json.dumps({
                    'manifest': manifest, 'manifest_sha256': m.fingerprint(manifest),
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

    def test_partial_minimum_override_cannot_pass_acceptance(self):
        with tempfile.TemporaryDirectory() as root, self.assertRaisesRegex(m.Refused, 'at least 2000'):
            m.report(root, 1)

    def test_mixed_host_binary_hashes_refused_with_same_launcher(self):
        with tempfile.TemporaryDirectory() as root:
            for tag, host_hash in [('a', 'old'), ('b', 'new')]:
                directory = Path(root) / tag
                directory.mkdir()
                manifest = {'schema': 1, 'source_commit': 'same', 'guest_sha256': 'same',
                            'host_binaries': {'wc-proxy': host_hash}}
                (directory / 'result.json').write_text(json.dumps({
                    'kind': 'batch', 'count': 1000, 'source_commit': 'same', 'guest_sha256': 'same',
                    'manifest': manifest, 'manifest_sha256': m.fingerprint(manifest),
                    'samples_ns': [[i * 1001000000, i * 1001000000 + 100000] for i in range(1000)]}))
            with self.assertRaisesRegex(m.Refused, 'mixed measurement manifests'):
                m.report(root, 2000)

    def test_transitioning_units_are_not_stopped(self):
        for state in ('active', 'activating', 'deactivating', 'reloading', 'maintenance'):
            self.assertFalse(m.terminal_units(f'wc-launch-wcil-1.service loaded {state} pending session'))
        for state in ('inactive', 'failed'):
            self.assertTrue(m.terminal_units(f'wc-launch-wcil-1.service loaded {state} dead session'))
        self.assertTrue(m.terminal_units(''))
        with self.assertRaises(m.Refused):
            m.terminal_units('manager is unavailable')

    def test_stop_waits_for_terminal_state_and_manager_errors_fail(self):
        h = m.Harness(config())
        h.guest = lambda *args, **kwargs: ''
        answers = iter(['wc-launch-wcil-1.service loaded deactivating stop session',
                        'wc-launch-wcil-1.service loaded inactive dead session'])
        h.host = lambda script: next(answers)
        with patch.object(m.time, 'sleep') as sleep:
            h.stop()
        sleep.assert_called_once()
        h.host = lambda script: (_ for _ in ()).throw(m.Refused('manager failed'))
        with self.assertRaisesRegex(m.Refused, 'manager failed'):
            h.stop()

    def test_proxy_and_compositor_pins_required(self):
        for key in ('host_proxy', 'compositor_sha256', 'guest_compositor'):
            c = config()
            del c[key]
            with self.subTest(key=key), self.assertRaises(m.Refused):
                m.validate(c)

    def test_existing_shared_compositor_must_match_pin(self):
        h = m.Harness(config())
        replies = iter([h.c['compositor_sha256'], 'f' * 64])
        h.guest = lambda script: next(replies)
        with self.assertRaisesRegex(m.Refused, 'existing shared compositor'):
            h.check_compositor()

    def test_manifest_tampering_is_refused(self):
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root) / 'one'
            directory.mkdir()
            manifest = {'schema': 1, 'host_binaries': {'proxy': 'first'}}
            digest = m.fingerprint(manifest)
            manifest['host_binaries']['proxy'] = 'changed'
            (directory / 'result.json').write_text(json.dumps({
                'kind': 'batch', 'manifest': manifest, 'manifest_sha256': digest,
                'count': 2000, 'samples_ns': [[0, 1]] * 2000}))
            with self.assertRaisesRegex(m.Refused, 'fingerprint mismatch'):
                m.report(root, 2000)

    def test_broker_key_metadata_requires_provisioned_privileged_key(self):
        acl = {'protected': True, 'owner': 'S-1-5-32-544',
               'allow': ['S-1-5-18', 'S-1-5-32-544']}
        valid = {'local_key': False, 'service': 'Running', 'length': 64,
                 'directory': acl.copy(), 'key': acl.copy()}
        m.broker_key_metadata(valid)
        invalid = [dict(valid, local_key=True), dict(valid, length=0),
                   dict(valid, service='Stopped'), {}]
        for field in ('directory', 'key'):
            for change in ({'owner': 'S-1-5-21-123-1001'}, {'protected': False},
                           {'allow': acl['allow'] + ['S-1-5-32-545']}):
                invalid.append(dict(valid, **{field: dict(acl, **change)}))
        for bad in invalid:
            with self.subTest(bad=bad), self.assertRaises(m.Refused):
                m.broker_key_metadata(bad)

    def test_unidentified_mapping_is_refused_even_with_identified_mapping(self):
        c = config()
        rows = (f"{c['server']} {c['guest_user']} wctest installs=wcil\n"
                f"{c['server']} - wctest installs=wcil\n")
        with self.assertRaisesRegex(m.Refused, 'unidentified'):
            m.check_mapping(rows, c)

    def test_signing_proof_requires_correct_authenticated_user(self):
        text = 'wc-compositor: boot: record signed by the broker in 53 us for os=windows user=test id=S-1-5-21-123-1001'
        m.signed_identity(text, 'test')
        for bad in ('', text.replace('user=test', 'user=other'),
                    text + '\nboot: broker did not sign; signing with the key file'):
            with self.subTest(bad=bad), self.assertRaises(m.Refused):
                m.signed_identity(bad, 'test')

    def test_denied_key_metadata_is_not_treated_as_provisioned(self):
        h = m.Harness(config())
        h.guest = lambda script: (_ for _ in ()).throw(m.Refused('access denied'))
        with self.assertRaisesRegex(m.Refused, 'access denied'):
            h.check_broker_key()

    def test_signing_evidence_is_read_after_saved_byte_offset(self):
        h = m.Harness(config())
        scripts = []
        fresh = ('boot: record signed by the broker in 1 us for os=windows user='
                 + h.c['guest_user'] + ' id=S-1-5-21-123-1001')
        def guest(script):
            scripts.append(script)
            return fresh
        h.guest = guest
        self.assertEqual(h.verify_signed_boot('compositor.log', {'length': 4321, 'prefix_sha256': 'a' * 64}), fresh)
        self.assertIn('$f.Seek(4321,', scripts[0])
        self.assertIn('Prefix-Hash $f 4321', scripts[0])
        self.assertIn('compositor log prefix changed', scripts[0])

    def test_existing_log_is_always_snapshotted_without_process_inference(self):
        h = m.Harness(config())
        scripts = []
        snapshot = {'length': 8000, 'prefix_sha256': 'a' * 64, 'file_id': '00000001:0000000000000002'}
        def guest(script):
            scripts.append(script)
            return json.dumps(snapshot)
        h.guest = guest
        _, actual = h.compositor_log_offset()
        self.assertEqual(actual, snapshot)
        self.assertNotIn('Get-CimInstance', scripts[0])
        self.assertIn('$length = $f.Length', scripts[0])
        self.assertIn('Prefix-Hash $f $length', scripts[0])

    def test_replaced_log_identity_is_checked_before_reading_suffix(self):
        h = m.Harness(config())
        def guest(script):
            guard = script.index("if ([LogIdentity]::Read($f) -ne '00000001:0000000000000002')")
            self.assertLess(guard, script.index('$f.Seek(8000,'))
            raise m.Refused('compositor log file replaced during measurement')
        h.guest = guest
        with self.assertRaisesRegex(m.Refused, 'file replaced'):
            h.verify_signed_boot('compositor.log', {
                'length': 8000, 'prefix_sha256': 'a' * 64, 'file_id': '00000001:0000000000000002'})

    def test_changed_or_truncated_log_refusal_prevents_stale_signing(self):
        h = m.Harness(config())
        def guest(script):
            self.assertIn('Prefix-Hash $f 8000', script)
            self.assertIn('$file.Length -lt $length', script)
            raise m.Refused('compositor log prefix changed during measurement')
        h.guest = guest
        with self.assertRaisesRegex(m.Refused, 'prefix changed'):
            h.verify_signed_boot('compositor.log', {'length': 8000, 'prefix_sha256': 'a' * 64})

    def test_key_probe_never_reads_secret_contents(self):
        h = m.Harness(config())
        scripts = []
        acl = {'protected': True, 'owner': 'S-1-5-32-544',
               'allow': ['S-1-5-18', 'S-1-5-32-544']}
        def guest(script):
            scripts.append(script)
            return json.dumps({'local_key': False, 'service': 'Running', 'length': 64,
                               'directory': acl, 'key': acl})
        h.guest = guest
        h.check_broker_key()
        self.assertIn('Get-Acl', scripts[0])
        for forbidden in ('Get-Content', 'Get-FileHash', '[IO.File]::Open', 'ReadAll'):
            self.assertNotIn(forbidden, scripts[0])

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
        self.manifest = {'schema': 1, 'identity': self.c.copy()}

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

    def compositor_log_offset(self):
        return 'compositor.log', 0

    def verify_signed_boot(self, log, offset):
        return 'authenticated fixture'

    def check_broker_key(self):
        return {}

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
