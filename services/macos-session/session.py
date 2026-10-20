"""launchd-owned, per-user native D-Bus/PulseAudio/portal lifetime."""
import asyncio
import fcntl
import json
import os
from pathlib import Path
import signal
import stat
import sys
from xml.sax.saxutils import escape
from urllib.parse import quote

from dbus_next import Message, MessageType
from bus import SessionBus as MessageBus

STARTUP_TIMEOUT = 30


def private_directory(path):
    path.mkdir(mode=0o700, parents=True, exist_ok=True)
    info = path.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise RuntimeError(f'Not a private directory owned by this user: {path}')


def runtime_directory():
    root = Path.home() / 'Library/Caches/waycompose'
    private_directory(root)
    root = root / f'session-{os.getuid()}'
    private_directory(root)
    return root


def bus_address(root):
    return 'unix:path=' + quote(str(root / 'bus'), safe='/._-')


def bus_config(root):
    return f'''<busconfig>
<type>session</type><listen>{escape(bus_address(root))}</listen>
<auth>EXTERNAL</auth><keep_umask/>
<policy context="default"><allow send_destination="*"/><allow receive_sender="*"/><allow own="*"/></policy>
</busconfig>'''


async def ready(root):
    bus = None
    try:
        info = (root / 'ready').lstat()
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
            return False
        bus = await MessageBus(bus_address=bus_address(root)).connect()
        for name in ('org.freedesktop.portal.Desktop', 'org.freedesktop.impl.portal.desktop.waycompose'):
            reply = await bus.call(Message(destination='org.freedesktop.DBus', path='/org/freedesktop/DBus',
                interface='org.freedesktop.DBus', member='GetConnectionUnixUser', signature='s', body=[name]))
            if reply.message_type == MessageType.ERROR or reply.body != [os.getuid()]:
                return False
        reader, writer = await asyncio.open_unix_connection(str(root / 'pulse/native'))
        writer.close()
        await writer.wait_closed()
        return True
    except (OSError, EOFError):
        return False
    finally:
        if bus:
            bus.disconnect()


async def wait_for_startup(check, children, deadline):
    """Race readiness against child exit within one monotonic budget."""
    probe = None
    exits = []
    try:
        if any(child.returncode is not None for child in children):
            raise RuntimeError('Native service exited before readiness')
        exits = [asyncio.create_task(child.wait()) for child in children]
        async with asyncio.timeout_at(deadline):
            while True:
                probe = asyncio.create_task(check())
                done, _ = await asyncio.wait([probe, *exits], return_when=asyncio.FIRST_COMPLETED)
                if any(task in done for task in exits) or any(child.returncode is not None for child in children):
                    raise RuntimeError('Native service exited before readiness')
                if await probe:
                    return
                probe = None
                await asyncio.sleep(0.05)
    except TimeoutError as error:
        raise RuntimeError('Native services did not become ready within the startup deadline') from error
    finally:
        tasks = exits + ([probe] if probe is not None else [])
        for task in tasks:
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)


async def launchctl(*arguments, deadline):
    spawning = asyncio.create_task(asyncio.create_subprocess_exec('/bin/launchctl', *arguments,
        stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL))
    process = None
    try:
        async with asyncio.timeout_at(deadline):
            process = await asyncio.shield(spawning)
            return await process.wait()
    finally:
        # A cancelled spawn may already own a child. Reap that specific child;
        # never terminate the service registered with launchd.
        if process is None:
            process = await spawning
        if process.returncode is None:
            process.kill()
            await process.wait()


async def ensure():
    if sys.platform != 'darwin':
        raise SystemExit('Native macOS services only')
    root = runtime_directory()
    deadline = asyncio.get_running_loop().time() + STARTUP_TIMEOUT
    try:
        async with asyncio.timeout_at(deadline):
            try:
                if await asyncio.wait_for(ready(root), 2):
                    print(root)
                    return
            except TimeoutError:
                pass
            domain = f'gui/{os.getuid()}'
            service = domain + '/org.waycompose.session-services'
            if await launchctl('print', service, deadline=deadline) != 0:
                if await launchctl('bootstrap', domain, os.environ['WC_SESSION_PLIST'], deadline=deadline) != 0:
                    # Concurrent application launch may already have registered it.
                    if await launchctl('print', service, deadline=deadline) != 0:
                        raise SystemExit('Cannot register native user services with launchd')
            await launchctl('kickstart', service, deadline=deadline)
            await wait_for_startup(lambda: ready(root), [], deadline)
            print(root)
    except TimeoutError as error:
        raise SystemExit('Native services did not become ready within the startup deadline') from error


async def main():
    if sys.platform != 'darwin':
        raise SystemExit('Native macOS services only')
    root = runtime_directory()
    private_directory(root / 'pulse')
    if len(os.fsencode(root / 'pulse/native')) > 103:
        raise SystemExit('Native socket path exceeds sockaddr_un capacity')
    lock = open(root / 'lock', 'a')
    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    os.umask(0o077)
    cookie = root / 'pulse/cookie'
    if not cookie.exists():
        with cookie.open('xb') as output:
            output.write(os.urandom(256))
    info = cookie.lstat()
    if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077 or info.st_size != 256:
        raise RuntimeError('Invalid private PulseAudio cookie')
    (root / 'ready').unlink(missing_ok=True)
    for endpoint in (root / 'bus', root / 'pulse/native'):
        if endpoint.exists():
            info = endpoint.lstat()
            if not stat.S_ISSOCK(info.st_mode) or info.st_uid != os.getuid():
                raise RuntimeError(f'Refusing to remove non-socket: {endpoint}')
            endpoint.unlink()
    config = root / 'bus.conf'
    config.write_text(bus_config(root))
    pulse_config = root / 'pulse/default.pa'
    # Keep native PulseAudio shared memory enabled. hl must supply shared host
    # mappings and descriptor passing; disabling shm would hide a runtime bug.
    # Foreign imported-pool forwarding fails the destination connection until
    # the shipped peers negotiate original-page leases and whole-pool rights.
    pulse_config.write_text(f'''load-module module-native-protocol-unix socket={json.dumps(str(root / "pulse/native"), ensure_ascii=False)} auth-cookie={json.dumps(str(cookie), ensure_ascii=False)} auth-anonymous=no strict-original-pool=yes
load-module module-coreaudio-detect
''')
    env = dict(os.environ, DBUS_SESSION_BUS_ADDRESS=bus_address(root),
               PULSE_SERVER=f'unix:{root}/pulse/native', PULSE_COOKIE=str(cookie))
    stop = asyncio.Event()
    loop = asyncio.get_running_loop()
    for sig in (signal.SIGINT, signal.SIGTERM):
        loop.add_signal_handler(sig, stop.set)
    children = []
    waiters = []
    bus = None
    deadline = loop.time() + STARTUP_TIMEOUT
    try:
        for argv in ([os.environ['WC_DBUS_DAEMON'], '--nofork', f'--config-file={config}'],
                     [os.environ['WC_PULSEAUDIO'], '--daemonize=no', '--use-pid-file=no',
                      '--exit-idle-time=-1', '--disallow-exit', '-n', f'--file={pulse_config}']):
            children.append(await asyncio.create_subprocess_exec(*argv, env=env))
        async def sockets_ready():
            return (root / 'bus').exists() and (root / 'pulse/native').exists()
        await wait_for_startup(sockets_ready, children, deadline)
        children.append(await asyncio.create_subprocess_exec(sys.executable, os.environ['WC_PORTAL'], env=env))
        async def connect_bus():
            nonlocal bus
            bus = await MessageBus(bus_address=env['DBUS_SESSION_BUS_ADDRESS']).connect()
            return True
        await wait_for_startup(connect_bus, children, deadline)
        async def portal_ready():
            for name in ('org.freedesktop.portal.Desktop', 'org.freedesktop.impl.portal.desktop.waycompose'):
                reply = await bus.call(Message(destination='org.freedesktop.DBus', path='/org/freedesktop/DBus',
                    interface='org.freedesktop.DBus', member='NameHasOwner', signature='s', body=[name]))
                if reply.message_type == MessageType.ERROR or reply.body != [True]:
                    return False
            return True
        await wait_for_startup(portal_ready, children, deadline)
        # Publish only after every service is ready. The application factory must
        # still verify socket connectivity, since service death removes this file.
        ready = root / 'ready'
        pending_ready = root / 'ready.new'
        pending_ready.write_text('\n'.join(f'{key}={env[key]}' for key in ('DBUS_SESSION_BUS_ADDRESS', 'PULSE_SERVER', 'PULSE_COOKIE')) + '\n')
        pending_ready.replace(ready)
        waiters = [asyncio.create_task(child.wait()) for child in children]
        waiters.append(asyncio.create_task(stop.wait()))
        await asyncio.wait(waiters, return_when=asyncio.FIRST_COMPLETED)
    finally:
        (root / 'ready').unlink(missing_ok=True)
        if bus:
            bus.disconnect()
        for child in reversed(children):
            if child.returncode is None:
                child.terminate()
        for child in reversed(children):
            try:
                await asyncio.wait_for(child.wait(), 5)
            except asyncio.TimeoutError:
                child.kill()
                await child.wait()
        for waiter in waiters:
            waiter.cancel()


if __name__ == '__main__':
    if sys.argv[1:] not in ([], ['--ensure']):
        raise SystemExit('Usage: wc-macos-session-services [--ensure]')
    asyncio.run(ensure() if sys.argv[1:] else main())
