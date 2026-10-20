"""Linux/offline D-Bus contract tests. Never calls macOS GUI or native services."""
import asyncio
import importlib.util
import os
from pathlib import Path
import shutil
import tempfile
import unittest

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
                             ('https://example.invalid/\n', {}), ('https://example.invalid/', {'writable': Variant('b', True)}),
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

    async def test_duplicate_request_token_is_rejected(self):
        options = {'handle_token': Variant('s', 'same')}
        self.assertEqual((await self.open(options=options)).message_type, MessageType.METHOD_RETURN)
        self.assertEqual((await self.open(options=options)).error_name, 'org.freedesktop.portal.Error.InvalidArgument')


if __name__ == '__main__':
    # Test HOME and every XDG directory are private even though no app runs.
    with tempfile.TemporaryDirectory(dir='/mnt/datasets', prefix='wc-portal-test-') as home:
        for key in ('HOME', 'XDG_CONFIG_HOME', 'XDG_DATA_HOME', 'XDG_STATE_HOME', 'XDG_CACHE_HOME', 'XDG_RUNTIME_DIR'):
            target = Path(home, key)
            target.mkdir(mode=0o700)
            os.environ[key] = str(target)
        os.environ.pop('DBUS_SESSION_BUS_ADDRESS', None)
        unittest.main()
