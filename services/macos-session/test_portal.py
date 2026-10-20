"""Linux/offline D-Bus contract tests. Never calls macOS GUI or native services."""
import asyncio
import importlib.util
import os
from pathlib import Path
import shutil
import tempfile
import unittest
from unittest.mock import AsyncMock, patch

from dbus_next import Message, MessageType, Variant
from bus import SessionBus as MessageBus
import session as native_session

spec = importlib.util.spec_from_file_location('portal', Path(__file__).with_name('portal.py'))
portal = importlib.util.module_from_spec(spec)
spec.loader.exec_module(portal)


class Host:
    def __init__(self):
        self.opened = []
        self.proceed = asyncio.Event()
        self.started = asyncio.Event()
        self.cancelled = asyncio.Event()
        self.result = 0

    async def settings(self):
        return {portal.SETTINGS: {'color-scheme': Variant('u', 1)}}

    async def open_uri(self, uri):
        self.started.set()
        try:
            await self.proceed.wait()
            self.opened.append(uri)
            return self.result
        except asyncio.CancelledError:
            self.cancelled.set()
            raise


class Contract(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        base = '/mnt/datasets/builds/evidence/macos-session-services'
        Path(base).mkdir(parents=True, exist_ok=True)
        self.temp = tempfile.TemporaryDirectory(prefix='bus-', dir=base)
        self.address = 'unix:path=' + self.temp.name + '/bus'
        config = Path(self.temp.name, 'bus.conf')
        config.write_text(native_session.bus_config(Path(self.temp.name)))
        self.daemon = await asyncio.create_subprocess_exec(shutil.which('dbus-daemon'), '--nofork',
            '--config-file=' + str(config), stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL)
        for _ in range(100):
            if Path(self.temp.name, 'bus').exists():
                break
            await asyncio.sleep(0.01)
        self.server = await MessageBus(bus_address=self.address).connect()
        await self.server.request_name(portal.PUBLIC)
        await self.server.request_name(portal.BACKEND)
        self.host = Host()
        self.service = portal.Portal(self.server, self.host)
        await self.service.start()
        self.client = await MessageBus(bus_address=self.address).connect()
        self.other = await MessageBus(bus_address=self.address).connect()
        self.responses = asyncio.Queue()
        def response(message):
            if message.message_type == MessageType.SIGNAL and message.interface == 'org.freedesktop.portal.Request':
                self.responses.put_nowait(message)
        self.client.add_message_handler(response)

    async def asyncTearDown(self):
        await self.service.shutdown()
        for bus in (self.client, self.other, self.server):
            bus.disconnect()
            await bus.wait_for_disconnect()
        self.daemon.terminate()
        await self.daemon.wait()
        self.temp.cleanup()

    async def call(self, interface, member, signature='', body=None, path=portal.PATH, client=None, destination=portal.PUBLIC):
        return await (client or self.client).call(Message(destination=destination, path=path,
            interface=interface, member=member, signature=signature, body=body or []))

    async def open(self, uri='https://example.invalid/', options=None):
        return await self.call('org.freedesktop.portal.OpenURI', 'OpenURI', 'ssa{sv}', ['', uri, options or {}])

    async def test_unmodified_libdbus_client_authenticates(self):
        # This is an upstream Linux ELF client using its actual OS UID. It
        # verifies EXTERNAL/client discovery offline, not hl's Darwin mapping.
        process = await asyncio.create_subprocess_exec(shutil.which('dbus-send'),
            '--session', '--dest=' + portal.PUBLIC, '--print-reply', portal.PATH,
            'org.freedesktop.portal.Settings.ReadOne', 'string:' + portal.SETTINGS,
            'string:color-scheme', env=dict(os.environ, DBUS_SESSION_BUS_ADDRESS=self.address),
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
        output, errors = await process.communicate()
        self.assertEqual(process.returncode, 0, errors.decode())
        self.assertIn('uint32 1', output.decode())

    async def test_disconnect_closes_native_descriptors(self):
        connection = await MessageBus(bus_address=self.address).connect()
        connection.disconnect()
        await connection.wait_for_disconnect()
        self.assertEqual(connection._sock.fileno(), -1)
        connection.disconnect()  # repeated shutdown must be harmless

    async def test_settings_frontend_and_backend(self):
        for interface, destination in [('org.freedesktop.portal.Settings', portal.PUBLIC),
                                       ('org.freedesktop.impl.portal.Settings', portal.BACKEND)]:
            result = await self.call(interface, 'Read', 'ss', [portal.SETTINGS, 'color-scheme'], destination=destination)
            self.assertEqual(result.body, [Variant('v', Variant('u', 1)) if destination == portal.PUBLIC else Variant('u', 1)])
            result = await self.call(interface, 'ReadAll', 'as', [['org.freedesktop.*']], destination=destination)
            self.assertEqual(result.body, [await self.host.settings()])
        result = await self.call('org.freedesktop.portal.Settings', 'ReadOne', 'ss', [portal.SETTINGS, 'color-scheme'])
        self.assertEqual(result.body, [Variant('u', 1)])
        result = await self.call('org.freedesktop.portal.Settings', 'ReadAll', 'as', [['']])
        self.assertEqual(result.body, [await self.host.settings()])
        result = await self.call('org.freedesktop.portal.Settings', 'Read', 'ss', ['absent', 'absent'])
        self.assertEqual(result.error_name, 'org.freedesktop.portal.Error.NotFound')

    async def test_unsupported_interfaces_are_absent(self):
        result = await self.call('org.freedesktop.DBus.Introspectable', 'Introspect')
        self.assertIn('org.freedesktop.impl.portal.Settings', result.body[0])
        self.assertNotIn('ScreenCast', result.body[0])
        result = await self.call('org.freedesktop.portal.FileChooser', 'OpenFile')
        self.assertEqual(result.error_name, 'org.freedesktop.DBus.Error.UnknownMethod')

    async def test_open_is_asynchronous_and_preserves_result(self):
        self.host.result = 1  # native Cancel
        result = await self.open(options={'handle_token': Variant('s', 'chosen')})
        self.assertEqual(result.message_type, MessageType.METHOD_RETURN)
        path = result.body[0]
        self.assertTrue(path.endswith('/chosen'))
        await asyncio.wait_for(self.host.started.wait(), 2)
        self.assertEqual(self.host.opened, [])
        self.host.proceed.set()
        response = await asyncio.wait_for(self.responses.get(), 2)
        self.assertEqual(response.path, path)
        self.assertEqual(response.body, [1, {}])
        self.assertNotIn(path, self.service.requests)

    async def test_close_is_owned_and_cancels_pending_operation(self):
        result = await self.open()
        path = result.body[0]
        await asyncio.wait_for(self.host.started.wait(), 2)
        result = await self.call('org.freedesktop.portal.Request', 'Close', path=path, client=self.other)
        self.assertEqual(result.error_name, 'org.freedesktop.DBus.Error.AccessDenied')
        self.assertFalse(self.host.cancelled.is_set())
        result = await self.call('org.freedesktop.portal.Request', 'Close', path=path)
        self.assertEqual(result.message_type, MessageType.METHOD_RETURN)
        await asyncio.wait_for(self.host.cancelled.wait(), 2)
        self.assertEqual(self.host.opened, [])
        self.assertNotIn(path, self.service.requests)

    async def test_no_files_commands_or_unsupported_grants(self):
        for uri, options in [('file:///etc/passwd', {}), ('javascript:alert(1)', {}),
                             ('https://example.invalid/\n', {}), ('https://example.invalid/', {'ask': Variant('s', 'invalid')}),
                             ('https://example.invalid/', {'handle_token': Variant('s', '../../bad')})]:
            result = await self.open(uri, options)
            self.assertEqual(result.error_name, 'org.freedesktop.portal.Error.InvalidArgument')
        self.assertFalse(self.host.started.is_set())

    async def test_caller_exit_cancels_native_dialog(self):
        result = await self.call('org.freedesktop.portal.OpenURI', 'OpenURI', 'ssa{sv}',
            ['', 'https://example.invalid/', {}], client=self.other)
        self.assertEqual(result.message_type, MessageType.METHOD_RETURN)
        await asyncio.wait_for(self.host.started.wait(), 2)
        self.other.disconnect()
        await asyncio.wait_for(self.host.cancelled.wait(), 2)
        self.assertEqual(self.host.opened, [])

    async def test_service_shutdown_cancels_native_dialog(self):
        self.assertEqual((await self.open()).message_type, MessageType.METHOD_RETURN)
        await asyncio.wait_for(self.host.started.wait(), 2)
        await self.service.shutdown()
        self.assertTrue(self.host.cancelled.is_set())
        self.assertEqual(self.service.requests, {})
        self.assertEqual(self.host.opened, [])

    async def test_native_host_dispatches_default_opener_without_dialog(self):
        with patch.dict(os.environ, {'WC_PORTAL_OPENER': '/native/wc-portal-open'}):
            with patch.object(portal, 'command', new=AsyncMock(return_value=(0, ''))) as invoke:
                self.assertEqual(await portal.NativeHost().open_uri('https://example.invalid/'), 0)
                invoke.assert_awaited_once_with('/native/wc-portal-open', 'https://example.invalid/')

    async def test_standard_open_options(self):
        for key in ('ask', 'writable'):
            result = await self.open(options={key: Variant('b', True)})
            self.assertEqual(result.error_name, 'org.freedesktop.DBus.Error.NotSupported')
        result = await self.open(options={'ask': Variant('b', False), 'writable': Variant('b', False),
            'activation_token': Variant('s', 'advisory-wayland-token')})
        self.assertEqual(result.message_type, MessageType.METHOD_RETURN)
        await asyncio.wait_for(self.host.started.wait(), 2)

    async def test_caller_disappearance_during_authentication_cancels_open(self):
        entered = asyncio.Event()
        release = asyncio.Event()
        original = self.server.call
        async def delayed_auth(message):
            result = await original(message)
            if message.member == 'GetConnectionUnixUser':
                entered.set()
                await release.wait()
            return result
        self.server.call = delayed_auth
        client_call = asyncio.create_task(self.call('org.freedesktop.portal.OpenURI', 'OpenURI',
            'ssa{sv}', ['', 'https://example.invalid/', {}], client=self.other))
        await asyncio.wait_for(entered.wait(), 2)
        pending = [task for task, owner in self.service.pending_owners.items()
                   if owner == self.other.unique_name]
        self.assertEqual(len(pending), 1)
        self.other.disconnect()
        await asyncio.wait_for(asyncio.gather(*pending, return_exceptions=True), 2)
        release.set()
        await asyncio.gather(client_call, return_exceptions=True)
        self.assertTrue(pending[0].cancelled())
        self.assertFalse(self.host.started.is_set())
        self.assertEqual(self.service.requests, {})

    async def test_reused_path_survives_delayed_old_done_callback(self):
        options = {'handle_token': Variant('s', 'reused')}
        first = await self.open(options=options)
        path = first.body[0]
        old_task = self.service.requests[path][1]
        original = self.service.forget_request
        delayed = []
        def defer_old_callback(path, task):
            if task is old_task and task.done():
                delayed.append((path, task))
            else:
                original(path, task)
        self.service.forget_request = defer_old_callback
        self.host.proceed.set()
        await asyncio.wait_for(self.responses.get(), 2)
        await old_task
        await asyncio.sleep(0)
        self.assertEqual(delayed, [(path, old_task)])
        self.host.proceed.clear()
        second = await self.open(options=options)
        self.assertEqual(second.body, [path])
        replacement = self.service.requests[path][1]
        original(*delayed.pop())
        self.assertIs(self.service.requests[path][1], replacement)
        info = await self.call('org.freedesktop.DBus.Introspectable', 'Introspect', path=path)
        self.assertIn('org.freedesktop.portal.Request', info.body[0])
        closed = await self.call('org.freedesktop.portal.Request', 'Close', path=path)
        self.assertEqual(closed.message_type, MessageType.METHOD_RETURN)

    async def test_cancel_before_first_execution_removes_request(self):
        path = '/org/freedesktop/portal/desktop/request/test/early'
        task = asyncio.create_task(self.service.finish(path, 'https://example.invalid/', self.client.unique_name))
        self.service.requests[path] = (self.client.unique_name, task)
        self.server.export(path, portal.Request())
        task.add_done_callback(lambda completed: self.service.forget_request(path, completed))
        task.cancel()
        await asyncio.gather(task, return_exceptions=True)
        self.assertNotIn(path, self.service.requests)

    async def test_duplicate_request_token_is_rejected(self):
        options = {'handle_token': Variant('s', 'same')}
        self.assertEqual((await self.open(options=options)).message_type, MessageType.METHOD_RETURN)
        self.assertEqual((await self.open(options=options)).error_name, 'org.freedesktop.portal.Error.InvalidArgument')


class Startup(unittest.IsolatedAsyncioTestCase):
    async def test_readiness_can_exceed_old_five_second_bound(self):
        loop = asyncio.get_running_loop()
        begin = loop.time()
        async def delayed():
            return loop.time() - begin >= 5.05
        await native_session.wait_for_startup(delayed, [], begin + native_session.STARTUP_TIMEOUT)
        self.assertGreaterEqual(loop.time() - begin, 5.05)

    async def test_deadline_cancels_a_blocked_probe(self):
        cancelled = asyncio.Event()
        async def blocked():
            try:
                await asyncio.Event().wait()
            finally:
                cancelled.set()
        with self.assertRaisesRegex(RuntimeError, 'startup deadline'):
            await native_session.wait_for_startup(blocked, [], asyncio.get_running_loop().time() + 0.03)
        self.assertTrue(cancelled.is_set())

    async def test_early_child_exit_never_publishes_readiness(self):
        class Exited:
            returncode = 1
        probe = AsyncMock(return_value=True)
        with self.assertRaisesRegex(RuntimeError, 'exited before readiness'):
            await native_session.wait_for_startup(probe, [Exited()], asyncio.get_running_loop().time() + 1)
        probe.assert_not_awaited()


    async def test_child_exit_cancels_blocked_readiness(self):
        exited = asyncio.Event()
        cancelled = asyncio.Event()
        class Child:
            returncode = None
            async def wait(self):
                await exited.wait()
                self.returncode = 1
                return 1
        async def blocked():
            try:
                await asyncio.Event().wait()
            finally:
                cancelled.set()
        loop = asyncio.get_running_loop()
        loop.call_later(0.02, exited.set)
        begin = loop.time()
        with self.assertRaisesRegex(RuntimeError, 'exited before readiness'):
            await native_session.wait_for_startup(blocked, [Child()], begin + 1)
        self.assertTrue(cancelled.is_set())
        self.assertLess(loop.time() - begin, 0.5)

    async def test_stalled_launchctl_is_killed_and_reaped(self):
        stopped = asyncio.Event()
        class ControlChild:
            returncode = None
            killed = False
            waits = 0
            async def wait(self):
                self.waits += 1
                await stopped.wait()
                self.returncode = -9
                return self.returncode
            def kill(self):
                self.killed = True
                stopped.set()
        child = ControlChild()
        with patch.object(native_session.asyncio, 'create_subprocess_exec', new=AsyncMock(return_value=child)) as spawn:
            with self.assertRaises(TimeoutError):
                await native_session.launchctl('print', 'private-test', deadline=asyncio.get_running_loop().time() + 0.03)
        self.assertTrue(child.killed)
        self.assertEqual(child.returncode, -9)
        self.assertEqual(child.waits, 2)
        spawn.assert_awaited_once()

    async def test_ensure_budget_includes_initial_probe(self):
        async def blocked(root):
            await asyncio.Event().wait()
        with patch.object(native_session.sys, 'platform', 'darwin'), \
             patch.object(native_session, 'STARTUP_TIMEOUT', 0.03), \
             patch.object(native_session, 'runtime_directory', return_value=Path('/unused-private-fixture')), \
             patch.object(native_session, 'ready', new=blocked), \
             patch.object(native_session, 'launchctl', new=AsyncMock()) as invoke:
            with self.assertRaisesRegex(SystemExit, 'startup deadline'):
                await native_session.ensure()
        invoke.assert_not_awaited()


if __name__ == '__main__':
    # Test HOME and every XDG directory are private even though no app runs.
    with tempfile.TemporaryDirectory(dir='/mnt/datasets', prefix='wc-portal-test-') as home:
        for key in ('HOME', 'XDG_CONFIG_HOME', 'XDG_DATA_HOME', 'XDG_STATE_HOME', 'XDG_CACHE_HOME', 'XDG_RUNTIME_DIR'):
            target = Path(home, key)
            target.mkdir(mode=0o700)
            os.environ[key] = str(target)
        os.environ.pop('DBUS_SESSION_BUS_ADDRESS', None)
        unittest.main()
