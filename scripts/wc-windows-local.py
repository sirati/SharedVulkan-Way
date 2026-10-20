#!/usr/bin/env python3
"""Run two native Windows Wayland processes locally, with no VM or proxy.

On Linux this uses a private Wine prefix and Xvfb. That checks the Windows
build under Wine, not real Windows or Cygwin's libwayland compatibility.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import selectors
import shutil
import signal
import subprocess
import tempfile
import time
import uuid


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def scoped_wine_processes(marker):
    """Pin matching Linux processes before reading their run marker."""
    found = []
    for entry in Path('/proc').glob('[0-9]*'):
        try:
            descriptor = os.pidfd_open(int(entry.name))
        except (OSError, ProcessLookupError):
            continue
        try:
            environment = (entry / 'environ').read_bytes().split(b'\0')
            if marker.encode() in environment:
                found.append(descriptor)
                descriptor = None
        except (OSError, PermissionError):
            pass
        finally:
            if descriptor is not None:
                os.close(descriptor)
    return found


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', type=Path)
    parser.add_argument('--evidence', type=Path, required=True)
    parser.add_argument('--work-root', type=Path, help='short writable temporary-directory parent')
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    native = os.name == 'nt'
    binaries = (args.bin_dir or root / 'target/x86_64-pc-windows-gnu/debug').resolve()
    evidence = args.evidence.resolve()
    require(not evidence.is_relative_to(root), 'evidence must be outside the repository')
    require(not evidence.exists(), 'use a new evidence directory for each run')
    executables = [binaries / name for name in ('wc-compositor.exe', 'wc-shmclient.exe')]
    require(all(path.is_file() for path in executables),
            'build wc-compositor and wc-shmclient for x86_64-pc-windows-gnu first')
    if not native:
        require(os.name == 'posix' and Path('/proc').is_dir() and hasattr(os, 'pidfd_open'),
                'Wine mode requires Linux pidfds for scoped cleanup')
        require(all(shutil.which(name) for name in ('wine', 'winepath', 'Xvfb')),
                'run inside the repository dev shell with wine, winepath and Xvfb')
    evidence.mkdir(parents=True)
    work_root = args.work_root or (None if native else Path('/mnt/datasets'))
    if work_root is not None:
        work_root.mkdir(parents=True, exist_ok=True)
    scratch = Path(tempfile.mkdtemp(prefix='wcl-', dir=work_root))
    marker = 'WC_LOCAL_RUN=' + uuid.uuid4().hex
    env = os.environ.copy()
    env['WC_LOCAL_RUN'] = marker.split('=', 1)[1]
    for key, suffix in [('HOME', 'home'), ('XDG_CONFIG_HOME', 'config'),
                        ('XDG_DATA_HOME', 'data'), ('XDG_STATE_HOME', 'state'),
                        ('XDG_CACHE_HOME', 'cache'), ('XDG_RUNTIME_DIR', 'runtime'),
                        ('LOCALAPPDATA', 'local')]:
        path = scratch / suffix
        path.mkdir(mode=0o700)
        env[key] = str(path)
    env.pop('DBUS_SESSION_BUS_ADDRESS', None)
    env.update(WINEPREFIX=str(scratch / 'p'), WINEDEBUG='-all',
               WINEDLLOVERRIDES='mscoree,mshtml=')
    children = []
    files = []
    cleanup_ok = True
    result = None
    try:
        def start(argv, name, **options):
            log = (evidence / name).open('wb')
            files.append(log)
            child = subprocess.Popen(argv, env=env, stdout=log, stderr=subprocess.STDOUT, **options)
            children.append(child)
            return child

        if not native:
            # Xvfb chooses an unused display. No global display number or user screen.
            read_fd, write_fd = os.pipe()
            try:
                start(['Xvfb', '-displayfd', str(write_fd), '-screen', '0', '1024x768x24',
                       '-nolisten', 'tcp', '-ac'], 'xvfb.log', pass_fds=(write_fd,))
            finally:
                os.close(write_fd)
            try:
                with selectors.DefaultSelector() as selector:
                    selector.register(read_fd, selectors.EVENT_READ)
                    require(selector.select(10), 'Xvfb did not select a display')
                    display = os.read(read_fd, 64).decode().strip()
                    require(display.isdigit(), 'Xvfb returned an invalid display')
                    env['DISPLAY'] = ':' + display
            finally:
                os.close(read_fd)
            boot = start(['wineboot', '-u'], 'wineboot.log')
            require(boot.wait(timeout=180) == 0, 'private Wine prefix initialization failed')

        def windows_path(path):
            if native:
                return str(path)
            return subprocess.check_output(['winepath', '-w', str(path)], env=env,
                                           text=True, timeout=15).strip()

        prefix = [] if native else ['wine']
        socket = 'wl-local-' + uuid.uuid4().hex[:12]
        if native:
            require(len(os.fsencode(str(Path(env['LOCALAPPDATA']) / 'waycompose' / socket))) < 108,
                    'Windows AF_UNIX path is too long; choose a short writable --work-root')
        log = evidence / 'server.log'
        server = start(prefix + [str(executables[0]), '--socket', socket, '--timeout', '25',
                                 '--once', '--expect-presents', '30', '--log', windows_path(log)], 'server-loader.log')
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            require(server.poll() is None, 'local Windows server exited during startup')
            # The Windows server reports its selected socket after binding it.
            if log.exists() and re.search(r'(?:listening|socket).*' + re.escape(socket),
                                          log.read_text(errors='replace'), re.IGNORECASE):
                break
            time.sleep(.1)
        else:
            raise RuntimeError('local Windows server did not bind its socket')
        env['WAYLAND_DISPLAY'] = socket
        client = start(prefix + [str(executables[1]), '--frames', '30', '--seconds', '12',
                                 '--partial-damage', '--title', 'Local Wayland'], 'client.log')
        require(client.wait(timeout=20) == 0, 'native Windows Wayland client failed')
        client_text = (evidence / 'client.log').read_text(errors='replace')
        server_text = log.read_text(errors='replace')
        require(re.search(r'PASS.*30 frame\(s\) committed', client_text), 'client did not commit all frames')
        require('xdg_toplevel created' in server_text and 'HWND' in server_text,
                'no native Windows toplevel was created')
        require(re.search(r'pixels served by wl_shm.*[1-9][0-9]* section, 0 shared-file, 0 arena-slice', server_text),
                'buffers did not use local named sections')
        server_code = server.wait(timeout=30)
        require(server_code == 0, f'server did not shut down cleanly after its client: exit {server_code}')
        server_text = log.read_text(errors='replace')
        require('30 frames presented' in server_text, 'server did not present all client frames')
        require(re.search(r'windows at exit: 1 created, 1 destroyed, 0 still held, 0 still alive, 0 client', server_text),
                'server did not release its window and client')
        result = {'topology': 'same-machine', 'runtime': 'Windows' if native else 'Wine',
                  'client': 'native MinGW wc-shmclient', 'frames': 30,
                  'buffers': 'local named sections', 'virtual_machine': False,
                  'cygwin_compatibility_proved': False,
                  'binaries': {path.name: hashlib.sha256(path.read_bytes()).hexdigest() for path in executables}}
    finally:
        for child in children:
            if child.poll() is None:
                child.terminate()
                try:
                    child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait(timeout=5)
        if not native:
            descriptors = scoped_wine_processes(marker)
            try:
                for descriptor in descriptors:
                    try:
                        signal.pidfd_send_signal(descriptor, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
            finally:
                for descriptor in descriptors:
                    os.close(descriptor)
            time.sleep(.2)
            remaining = scoped_wine_processes(marker)
            cleanup_ok = not remaining
            for descriptor in remaining:
                os.close(descriptor)
        for file in files:
            file.close()
        if cleanup_ok:
            shutil.rmtree(scratch)
        else:
            print('run processes remain; scratch retained at', scratch)
    require(cleanup_ok, 'run cleanup failed; no passing result recorded')
    (evidence / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
    print('PASS: local Windows Wayland server and native client, 30 frames, no VM')


if __name__ == '__main__':
    try:
        main()
    except (OSError, RuntimeError, subprocess.SubprocessError) as error:
        raise SystemExit(str(error))
