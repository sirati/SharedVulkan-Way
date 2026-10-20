"""Native per-user Waycompose portal. No privilege or transport helper.

Settings is both a real public portal and a selectable backend. OpenURI is
implemented by the frontend because there is no separate Linux portal broker
on Darwin. Every open requires an affirmative native dialog. Unsupported
interfaces are absent from introspection and return UnknownMethod.
"""
import asyncio
import fnmatch
import os
import re
import signal as os_signal
import sys
from urllib.parse import urlsplit

from dbus_next import Message, MessageType, Variant, DBusError
from bus import SessionBus as MessageBus
from dbus_next.constants import NameFlag, RequestNameReply
from dbus_next.service import ServiceInterface, method, signal, dbus_property, PropertyAccess

PATH = '/org/freedesktop/portal/desktop'
PUBLIC = 'org.freedesktop.portal.Desktop'
BACKEND = 'org.freedesktop.impl.portal.desktop.waycompose'
SETTINGS = 'org.freedesktop.appearance'


async def command(*args):
    spawning = asyncio.create_task(asyncio.create_subprocess_exec(
        *args, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL))
    try:
        process = await asyncio.shield(spawning)
    except asyncio.CancelledError:
        process = await spawning
        if process.returncode is None:
            process.terminate()
        await process.wait()
        raise
    try:
        out, _ = await process.communicate()
    except asyncio.CancelledError:
        if process.returncode is None:
            process.terminate()
        await process.wait()
        raise
    return process.returncode, out.decode().strip()


class NativeHost:
    async def settings(self):
        status, value = await command('/usr/bin/defaults', 'read', '-g', 'AppleInterfaceStyle')
        # macOS uses an absent preference for the light appearance.
        scheme = 1 if status == 0 and value == 'Dark' else 2
        return {SETTINGS: {'color-scheme': Variant('u', scheme)}}

    async def open_uri(self, uri):
        # Pass the untrusted URI as an argument, never as AppleScript source.
        script = '''on run argv
set answer to display dialog ("A Linux application wants to open this link:" & return & item 1 of argv) with title "Waycompose" buttons {"Cancel", "Open"} default button "Cancel" cancel button "Cancel"
return button returned of answer
on error number -128
return "Cancel"
end try
end run'''
        status, answer = await command('/usr/bin/osascript', '-e', script, uri)
        if status:
            return 2
        if answer != 'Open':
            return 1 if answer == 'Cancel' else 2
        status, _ = await command(os.environ['WC_PORTAL_OPENER'], uri)
        return 0 if status == 0 else 2


class Settings(ServiceInterface):
    def __init__(self, name, host):
        super().__init__(name)
        self.host = host

    @dbus_property(access=PropertyAccess.READ)
    def version(self) -> 'u':
        return 1

    @signal()
    def SettingChanged(self, namespace: 's', key: 's', value: 'v') -> 'ssv':
        return [namespace, key, value]

    @method()
    async def Read(self, namespace: 's', key: 's') -> 'v':
        settings = await self.host.settings()
        if namespace not in settings or key not in settings[namespace]:
            raise DBusError('org.freedesktop.portal.Error.NotFound', 'Unknown setting')
        value = settings[namespace][key]
        return value if self.name.startswith('org.freedesktop.impl.') else Variant('v', value)

    @method()
    async def ReadAll(self, namespaces: 'as') -> 'a{sa{sv}}':
        settings = await self.host.settings()
        return {name: values for name, values in settings.items()
                if not namespaces or '' in namespaces or any(fnmatch.fnmatchcase(name, pattern) for pattern in namespaces)}


class PublicSettings(Settings):
    @dbus_property(access=PropertyAccess.READ)
    def version(self) -> 'u':
        return 2

    @method()
    async def ReadOne(self, namespace: 's', key: 's') -> 'v':
        settings = await self.host.settings()
        if namespace not in settings or key not in settings[namespace]:
            raise DBusError('org.freedesktop.portal.Error.NotFound', 'Unknown setting')
        return settings[namespace][key]


class OpenURI(ServiceInterface):
    def __init__(self):
        super().__init__('org.freedesktop.portal.OpenURI')

    @dbus_property(access=PropertyAccess.READ)
    def version(self) -> 'u':
        return 1

    # Actual dispatch needs the authenticated sender and is in Portal.handle.
    @method()
    def OpenURI(self, parent_window: 's', uri: 's', options: 'a{sv}') -> 'o':
        raise DBusError('org.freedesktop.portal.Error.Failed', 'Missing sender')


class Request(ServiceInterface):
    def __init__(self):
        super().__init__('org.freedesktop.portal.Request')

    @method()
    def Close(self):
        raise DBusError('org.freedesktop.DBus.Error.AccessDenied', 'Request owner required')

    @signal()
    def Response(self, response: 'u', results: 'a{sv}') -> 'ua{sv}':
        return [response, results]


class Portal:
    def __init__(self, bus, host):
        self.bus = bus
        self.host = host
        self.requests = {}
        self.counter = 0
        self.tasks = set()
        bus.add_message_handler(self.handle)
        self.settings_interfaces = [PublicSettings('org.freedesktop.portal.Settings', host),
                                    Settings('org.freedesktop.impl.portal.Settings', host)]
        for interface in self.settings_interfaces:
            bus.export(PATH, interface)
        bus.export(PATH, OpenURI())

    async def watch_settings(self):
        previous = await self.host.settings()
        while True:
            await asyncio.sleep(2)
            current = await self.host.settings()
            for namespace, values in current.items():
                for key, value in values.items():
                    if previous.get(namespace, {}).get(key) != value:
                        for interface in self.settings_interfaces:
                            interface.SettingChanged(namespace, key, value)
            previous = current

    def spawn(self, operation):
        task = asyncio.create_task(operation)
        self.tasks.add(task)
        task.add_done_callback(self.tasks.discard)

    async def shutdown(self):
        tasks = list(self.tasks) + [task for _, task in self.requests.values()]
        for task in tasks:
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)

    async def start(self):
        reply = await self.bus.call(Message(destination='org.freedesktop.DBus', path='/org/freedesktop/DBus',
            interface='org.freedesktop.DBus', member='AddMatch', signature='s',
            body=["type='signal',sender='org.freedesktop.DBus',interface='org.freedesktop.DBus',member='NameOwnerChanged'"]))
        if reply.message_type == MessageType.ERROR:
            raise RuntimeError('Cannot watch caller lifetime')

    def handle(self, message):
        if (message.message_type == MessageType.SIGNAL and message.sender == 'org.freedesktop.DBus'
                and message.interface == 'org.freedesktop.DBus' and message.member == 'NameOwnerChanged'
                and message.signature == 'sss'):
            name, _, owner = message.body
            if not owner:
                for request_owner, task in list(self.requests.values()):
                    if request_owner == name:
                        task.cancel()
            return False
        if message.message_type != MessageType.METHOD_CALL:
            return False
        if message.path == PATH and message.interface == 'org.freedesktop.portal.OpenURI' and message.member == 'OpenURI':
            self.spawn(self.open_request(message))
            return True
        if message.interface == 'org.freedesktop.portal.Request' and message.member == 'Close':
            request = self.requests.get(message.path)
            if request is not None:
                if message.signature:
                    self.bus.send(Message.new_error(message, 'org.freedesktop.DBus.Error.InvalidArgs', 'Close takes no arguments'))
                    return True
                owner, task = request
                if owner != message.sender:
                    self.bus.send(Message.new_error(message, 'org.freedesktop.DBus.Error.AccessDenied', 'Request belongs to another caller'))
                else:
                    task.cancel()
                    self.bus.send(Message.new_method_return(message))
                return True
        return False

    async def open_request(self, message):
        try:
            if message.signature != 'ssa{sv}':
                raise DBusError('org.freedesktop.DBus.Error.InvalidArgs', 'Expected ssa{sv}')
            reply = await self.bus.call(Message(destination='org.freedesktop.DBus', path='/org/freedesktop/DBus',
                interface='org.freedesktop.DBus', member='GetConnectionUnixUser', signature='s', body=[message.sender]))
            if reply.message_type == MessageType.ERROR or reply.body != [os.getuid()]:
                raise DBusError('org.freedesktop.DBus.Error.AccessDenied', 'Caller must have the native user UID')
            _, uri, options = message.body
            # File grants, chooser persistence and writable grants need a document
            # portal. Do not imply those permissions exist by accepting options.
            if set(options) - {'handle_token'}:
                raise DBusError('org.freedesktop.portal.Error.InvalidArgument', 'Unsupported OpenURI option')
            parsed = urlsplit(uri)
            if parsed.scheme not in ('https', 'http', 'mailto') or any(ord(c) < 32 for c in uri):
                raise DBusError('org.freedesktop.portal.Error.InvalidArgument', 'Only http, https and mailto links are supported')
            token = options.get('handle_token')
            if token is not None and (token.signature != 's' or not re.fullmatch(r'[A-Za-z0-9_]+', token.value)):
                raise DBusError('org.freedesktop.portal.Error.InvalidArgument', 'Invalid handle_token')
            self.counter += 1
            token = token.value if token is not None else f'wc{self.counter}'
            sender = message.sender.removeprefix(':').replace('.', '_')
            path = f'/org/freedesktop/portal/desktop/request/{sender}/{token}'
            if path in self.requests:
                raise DBusError('org.freedesktop.portal.Error.InvalidArgument', 'Request already exists')
            task = asyncio.create_task(self.finish(path, uri, message.sender))
            self.requests[path] = (message.sender, task)
            self.bus.export(path, Request())
            task.add_done_callback(lambda completed: self.forget_request(path))
            self.bus.send(Message.new_method_return(message, 'o', [path]))
        except DBusError as error:
            self.bus.send(Message.new_error(message, error.type, error.text))
        except Exception:
            self.bus.send(Message.new_error(message, 'org.freedesktop.portal.Error.Failed', 'Native request failed'))

    def forget_request(self, path):
        if self.requests.pop(path, None) is not None:
            self.bus.unexport(path)

    async def finish(self, path, uri, sender):
        try:
            result = await self.host.open_uri(uri)
            self.bus.send(Message(destination=sender, path=path, interface='org.freedesktop.portal.Request',
                member='Response', message_type=MessageType.SIGNAL, signature='ua{sv}', body=[result, {}]))
        except asyncio.CancelledError:
            pass
        except Exception:
            self.bus.send(Message(destination=sender, path=path, interface='org.freedesktop.portal.Request',
                member='Response', message_type=MessageType.SIGNAL, signature='ua{sv}', body=[2, {}]))
        finally:
            self.forget_request(path)


async def main():
    if sys.platform != 'darwin':
        raise SystemExit('This service requires native macOS. Tests inject a host explicitly.')
    bus = await MessageBus().connect()
    portal = Portal(bus, NativeHost())
    await portal.start()
    for name in (PUBLIC, BACKEND):
        if await bus.request_name(name, NameFlag.DO_NOT_QUEUE) != RequestNameReply.PRIMARY_OWNER:
            raise SystemExit(f'Portal name already owned: {name}')
    portal.spawn(portal.watch_settings())
    stopped = asyncio.Event()
    loop = asyncio.get_running_loop()
    for number in (os_signal.SIGINT, os_signal.SIGTERM):
        loop.add_signal_handler(number, stopped.set)
    waits = [asyncio.create_task(bus.wait_for_disconnect()), asyncio.create_task(stopped.wait())]
    await asyncio.wait(waits, return_when=asyncio.FIRST_COMPLETED)
    await portal.shutdown()
    bus.disconnect()
    for task in waits:
        task.cancel()
    await asyncio.gather(*waits, return_exceptions=True)


if __name__ == '__main__':
    asyncio.run(main())
