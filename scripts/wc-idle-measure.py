#!/usr/bin/env python3
"""Measure an already deployed, dedicated system-layout test installation.

Management uses SSH; measured requests use the launcher's shared-memory ring.
This script never deploys binaries, changes services, or kills a compositor.
See docs/idle-measure.md for deployment prerequisites and split-lease use.
"""
import argparse
import base64
import configparser
import hashlib
import ntpath
import json
import os
from pathlib import Path
import re
import shlex
import statistics
import subprocess
import sys
import threading
import time


class Refused(RuntimeError):
    pass


def require(ok, message):
    if not ok:
        raise Refused(message)


def validate(c):
    for key in ('host', 'guest', 'root_argv', 'server', 'guest_user', 'linux_user',
                'install', 'guest_dir', 'guest_home', 'test_home', 'app_exec',
                'guest_sha256', 'guest_compositor', 'compositor_sha256', 'host_proxy', 'app_sha256', 'host_binaries', 'source_commit', 'repo',
                'lease_helper', 'guest_nat_ip', 'evidence'):
        require(c.get(key), f'missing configuration: {key}')
    for key in ('install', 'linux_user', 'server', 'guest_user'):
        require(re.fullmatch(r'[A-Za-z0-9_.-]+', c[key]), f'unsafe {key}')
    require(c['linux_user'] == 'wctest', 'dedicated installation must map to wctest')
    require(c['install'].startswith('wc') and c['install'] not in ('wcivs', 'wcmc', 'wclinux'),
            'refusing a shared installation')
    require(isinstance(c['root_argv'], list) and all(isinstance(x, str) for x in c['root_argv']),
            'root_argv must be an argv array')
    require(c['app_exec'].startswith('/nix/store/') and c['app_exec'].endswith('/bin/foot'),
            'this harness requires a pinned stock foot executable')
    require(re.fullmatch(r'/var/tmp/waycompose-tests/[A-Za-z0-9_-]+', c['test_home']), 'test_home must be a dedicated throwaway directory')
    require(Path(c['evidence']).is_absolute() and c['evidence'].startswith('/mnt/datasets/'),
            'evidence must be under /mnt/datasets')
    for key in ('guest_sha256', 'compositor_sha256', 'app_sha256'):
        require(re.fullmatch('[0-9a-f]{64}', c[key]), f'{key} must pin the approved binary')
    require(isinstance(c['host_binaries'], dict) and len(c['host_binaries']) >= 4, 'pin host daemon and wc-session hashes')
    for path, digest in c['host_binaries'].items():
        require(path.startswith('/') and re.fullmatch('[0-9a-f]{64}', digest), 'invalid host binary pin')
    require(c['host_proxy'].endswith('/wc-proxy') and c['host_proxy'] in c['host_binaries'], 'pin installed wc-proxy')
    require(re.fullmatch('[0-9a-f]{40}', c['source_commit']), 'source_commit must be a full commit hash')


def parse_samples(text, count):
    samples = []
    pending = None
    for line in text.splitlines():
        parts = line.split()
        require(len(parts) == 2 and parts[0].isdigit(), 'malformed launcher trace')
        stamp, event = int(parts[0]), parts[1]
        if event == 'L.send':
            require(pending is None, 'unanswered request in trace')
            pending = stamp
        elif event == 'L.recv':
            require(pending is not None and stamp >= pending, 'unpaired/nonmonotonic reply')
            require(not samples or pending >= samples[-1][1] + 950_000_000,
                    'requests were not isolated by a 1 second gap')
            samples.append((pending, stamp))
            pending = None
    require(pending is None and len(samples) == count, f'expected {count} complete samples, got {len(samples)}')
    return samples


def check_mapping(text, c):
    selected = []
    for line in text.splitlines():
        fields = line.split('#')[0].split()
        if len(fields) < 3 or fields[0] != c['server'] or fields[1] not in (c['guest_user'], '-'):
            continue
        installs = next((x[9:].split(',') for x in fields[3:] if x.startswith('installs=')), None)
        if installs is None or c['install'] in installs:
            require(fields[1] != '-', 'unidentified guest mapping can select the test install')
            selected.append(fields[2])
    require(selected == [c['linux_user']], 'users.conf mapping is absent, ambiguous, or wrong')


def broker_key_metadata(data):
    privileged = {'S-1-5-18', 'S-1-5-32-544'}
    require(data.get('local_key') is False, 'user-profile session.key must be absent')
    require(data.get('service') == 'Running', 'Windows broker is not running')
    require(data.get('length') == 64, 'broker-held key is absent or unprovisioned')
    for name in ('directory', 'key'):
        acl = data.get(name, {})
        require(acl.get('protected') is True, f'broker {name} DACL must disable inheritance')
        require(acl.get('owner') in privileged, f'broker {name} must have a privileged owner')
        allow = acl.get('allow')
        require(isinstance(allow, list) and set(allow) == privileged,
                f'broker {name} must allow only SYSTEM and Administrators')


def signed_identity(text, user):
    require(re.search(r'boot: record signed by the broker in \d+ us for os=windows user='
                      + re.escape(user) + r' id=S-1-[0-9-]+(?:\s|$)', text),
            'fresh compositor log has no authenticated Windows broker signing identity')
    require('signing with the key file' not in text and 'broker did not sign' not in text,
            'compositor fell back to an unidentified signing key')


def terminal_units(text):
    """list-units --all --plain has UNIT LOAD ACTIVE SUB DESCRIPTION columns."""
    for line in text.splitlines():
        fields = line.split()
        require(len(fields) >= 4 and fields[0].endswith(('.service', '.socket', '.scope')),
                'malformed manager response while checking teardown')
        if fields[2] not in ('inactive', 'failed'):
            return False
    return True


def fingerprint(manifest):
    return hashlib.sha256(json.dumps(manifest, sort_keys=True, separators=(',', ':')).encode()).hexdigest()


def check_app(text, c):
    ini = configparser.ConfigParser(strict=False, interpolation=None)
    ini.read_string(text)
    require(ini.has_section('foot'), 'stock foot app missing')
    require(ini['foot'].get('exec', '').strip() == c['app_exec'], 'foot executable differs from approved stock build')
    require(not ini['foot'].get('args', '').strip(), 'foot must use default arguments')
    # wc-session also reads a test-home marker, checked separately. Reject
    # per-app overrides that could replace that isolation or disable a sandbox.
    for line in text.splitlines():
        if re.match(r'\s*env\s*=', line):
            value = line.split('=', 1)[1].strip()
            require(value in {f'{key}={c["test_home"]}/{suffix}' for key, suffix in
                              [('HOME', 'home'), ('XDG_CONFIG_HOME', 'config'),
                               ('XDG_DATA_HOME', 'data'), ('XDG_STATE_HOME', 'state'),
                               ('XDG_CACHE_HOME', 'cache')]}, 'unapproved app environment override')


class Harness:
    def __init__(self, c):
        validate(c)
        self.c = c
        self.failed = threading.Event()
        self.done = threading.Event()
        self.log = None
        self.manifest = None

    def command(self, argv, data=None, timeout=60):
        p = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        started = time.monotonic()
        first = True
        while True:
            try:
                out, err = p.communicate(data if first else None, timeout=1)
                break
            except subprocess.TimeoutExpired:
                first = False
                if self.failed.is_set() or time.monotonic() - started > timeout:
                    p.kill()
                    p.communicate()
                    raise Refused('command aborted after health failure or timeout; no retry')
        if self.log:
            self.log.write(json.dumps({'argv': argv, 'returncode': p.returncode,
                                       'stdout': out, 'stderr': err}) + '\n')
            self.log.flush()
        require(p.returncode == 0, f'command failed ({p.returncode}): {shlex.join(argv)}\n{err}')
        return out

    def host(self, script):
        argv = ['ssh', '-oBatchMode=yes', '-oConnectTimeout=10', self.c['host'],
                shlex.join(self.c['root_argv'] + ['bash', '-se'])]
        return self.command(argv, script)

    def guest(self, script, timeout=60):
        encoded = base64.b64encode(("$ErrorActionPreference='Stop';\n" + script).encode('utf-16le')).decode()
        return self.command(['ssh', '-oBatchMode=yes', '-oConnectTimeout=10', self.c['guest'],
                             'powershell.exe -NoProfile -NonInteractive -EncodedCommand ' + encoded], timeout=timeout)

    @staticmethod
    def ps(value):
        return "'" + value.replace("'", "''") + "'"

    def paths(self):
        c = self.c
        exe = c['guest_dir'].rstrip('\\/') + '\\wc-launch.exe'
        log = c['guest_home'].rstrip('\\/') + '\\AppData\\Local\\waycompose\\wc-launch-' + c['install'] + '.log'
        return exe, log

    def health(self):
        self.host('ping -c 1 -W 3 -- ' + shlex.quote(self.c['guest_nat_ip']) + ' >/dev/null\n')
        require('alive' in self.guest("Write-Output 'alive'"), 'guest SSH health failed')

    def preflight(self):
        c = self.c
        for ancestor in ('a160f0a', 'b8886dd'):
            self.command(['git', '-C', c['repo'], 'merge-base', '--is-ancestor', ancestor, c['source_commit']])
        # Require migrated system services, with direct hub rings enabled.
        for unit in ('wc-bell-hub', 'wc-sessiond'):
            state = self.host(f'systemctl is-active {unit}\nsystemctl show -p User --value {unit}\n')
            require(state.split() == ['active', 'waycompose'], f'{unit} is not the migrated system service')
        for path, digest in c['host_binaries'].items():
            require(self.host('sha256sum -- ' + shlex.quote(path) + '\n').split()[0] == digest,
                    f'host binary differs from approved build: {path}')
        hub = self.host('systemctl show -p ExecStart --value wc-bell-hub\n')
        sessiond = self.host('systemctl show -p ExecStart --value wc-sessiond\n')
        require('rings=' in hub, 'hub lacks direct rings')
        require(any(path in hub for path in c['host_binaries'] if path.endswith('/sv-hostd')),
                'live hub is not a pinned binary')
        require(any(path in sessiond for path in c['host_binaries'] if path.endswith('/wc-arena-host')),
                'live sessiond is not a pinned binary')
        check_mapping(self.host('cat /etc/waycompose/users.conf\n'), c)
        base = '/var/lib/waycompose/installs/' + c['install']
        require(self.host('cat ' + shlex.quote(base + '/test-home') + '\n').strip() == c['test_home'],
                'installed test-home marker differs; deployment isolation is not installed')
        require(base + '/wc-session' in c['host_binaries'], 'pin installed wc-session as well as daemons')
        home = shlex.quote(c['test_home'])
        require(self.host(f'readlink -f -- {home}\n').strip() == c['test_home'], 'test-home resolves through a symlink')
        require(self.host(f'stat -c %U -- {home}\n').strip() == 'wctest', 'test-home must be owned by wctest')
        for child in ('home', 'config', 'data', 'state', 'cache'):
            target = c['test_home'] + '/' + child
            quoted = shlex.quote(target)
            require(self.host(f'readlink -f -- {quoted}\nstat -c %U -- {quoted}\n').splitlines()
                    == [target, 'wctest'], f'test {child} directory is missing, redirected, or has wrong owner')
        check_app(self.host('cat ' + shlex.quote(base + '/apps.ini') + '\n'), c)
        digest = self.host('sha256sum -- ' + shlex.quote(c['app_exec']) + '\n').split()[0]
        require(digest == c['app_sha256'], 'stock app hash differs')
        # Refuse an existing dedicated session rather than stopping someone else's run.
        units = self.host("runuser -u wctest -- env XDG_RUNTIME_DIR=/run/user/$(id -u wctest) systemctl --user list-units --no-legend 'wc-launch-*'\n")
        require(not units.strip(), 'wctest has existing sessions; leave them untouched')
        exe, _ = self.paths()
        processes = self.guest(f"@(Get-CimInstance Win32_Process | Where-Object {{ $_.ExecutablePath -eq {self.ps(exe)} }}).Count")
        require(processes.strip() == '0', 'dedicated guest launcher already runs; leave it untouched')
        script = f"$h=(Get-FileHash -Algorithm SHA256 -LiteralPath {self.ps(exe)}).Hash; Write-Output $h\n"
        require(self.guest(script).strip().lower() == c['guest_sha256'], 'guest launcher differs from approved broker-compatible build')
        key_metadata = self.check_broker_key()
        ini = self.guest(f"Get-Content -Raw -LiteralPath {self.ps(c['guest_dir'] + '/wc-launch.ini')}")
        guest_ini = configparser.ConfigParser(interpolation=None)
        guest_ini.read_string(ini)
        require(guest_ini.get('session', 'debug-ssh-transport', fallback='off').strip() == 'off',
                'guest enables SSH product transport')
        require(guest_ini.getint('session', 'linger-secs', fallback=60) >= 900,
                'test session linger-secs must be at least 900 for split batches')
        require(guest_ini.get('session', 'wayland-bell', fallback='').strip(), 'guest has no Wayland doorbell')
        require(ntpath.normcase(ntpath.normpath(guest_ini.get('session', 'compositor', fallback='')))
                == ntpath.normcase(ntpath.normpath(c['guest_compositor'])), 'guest ini selects an unpinned compositor')
        require(guest_ini.get('session', 'installation', fallback='').strip() == c['install'], 'guest ini selects another installation')
        require(not guest_ini.get('session', 'session-key', fallback='').strip(), 'session-key override bypasses broker-only signing')
        self.check_compositor()
        # Snapshot only immutable inputs, excluding dynamic PIDs/state and sample counts.
        identity_keys = ('source_commit', 'server', 'guest_user', 'linux_user', 'install',
                         'guest_dir', 'guest_home', 'test_home', 'app_exec', 'app_sha256',
                         'guest_sha256', 'guest_compositor', 'compositor_sha256', 'host_proxy', 'host_binaries')
        manifest = {'schema': 1, 'broker_key_metadata': key_metadata, 'identity': {key: c[key] for key in identity_keys},
                    'guest_ini_sha256': hashlib.sha256(ini.encode()).hexdigest(),
                    'gap_ms': 1000, 'instrument': 'guest QPC L.send/L.recv',
                    'harness_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                    'guest_wc_environment': self.guest("Get-ChildItem Env: | Where-Object { $_.Name -in @('WC_CTL_SPIN_US','WC_LAUNCH_SPIN_US','WC_BELL_INLINE') } | Sort-Object Name | ForEach-Object { $_.Name + '=' + $_.Value }")}
        manifest['host_configuration'] = self.host(
            'systemctl cat wc-bell-hub wc-sessiond\n'
            'systemctl show -p ExecStart -p Environment wc-bell-hub wc-sessiond\n'
            'cat /etc/waycompose/users.conf\n'
            + 'cat ' + shlex.quote(base + '/apps.ini') + ' ' + shlex.quote(base + '/test-home') + '\n'
            + 'if test -f ' + shlex.quote(base + '/gpu.env') + '; then cat '
            + shlex.quote(base + '/gpu.env') + '; fi\n')
        # JSON roundtrip prevents later config mutations from altering the snapshot.
        self.manifest = json.loads(json.dumps(manifest))
        self.health()

    def check_broker_key(self):
        # Administrative management-channel metadata only. Never Get-Content,
        # Get-FileHash, or open a stream on either signing-key file.
        local = self.ps(self.c['guest_dir'].rstrip('\\/') + '\\session.key')
        script = r"""
function Acl-Metadata($path) {
  $acl = Get-Acl -LiteralPath $path
  $allow = @($acl.Access | Where-Object { $_.AccessControlType -eq 'Allow' } |
    ForEach-Object { $_.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value } |
    Sort-Object -Unique)
  return @{protected=$acl.AreAccessRulesProtected; allow=$allow; owner=$acl.GetOwner([Security.Principal.SecurityIdentifier]).Value}
}
$dir = Join-Path $env:ProgramData 'waycompose\keys'
""" + f"$key = Join-Path $dir {self.ps(self.c['install'] + '.key')}\n" + f"""
@{{local_key=(Test-Path -LiteralPath {local});
  service=(Get-Service -Name 'waycompose-broker').Status.ToString();
  length=(Get-Item -LiteralPath $key).Length;
  directory=(Acl-Metadata $dir); key=(Acl-Metadata $key)}} | ConvertTo-Json -Depth 4 -Compress
"""
        data = json.loads(self.guest(script))
        broker_key_metadata(data)
        return data

    def compositor_log_offset(self):
        log = self.c['guest_home'].rstrip('\\/') + '\\AppData\\Local\\waycompose\\wc-compositor.log'
        script = f"""
$p = @(Get-CimInstance Win32_Process -Filter "Name='sv-guest.exe'" | Where-Object {{ $_.CommandLine -like '*--user-control*' }})
if ($p.Count -gt 0 -and (Test-Path -LiteralPath {self.ps(log)})) {{
  Write-Output (Get-Item -LiteralPath {self.ps(log)}).Length
}} else {{ Write-Output 0 }}
"""
        return log, int(self.guest(script).strip())

    def verify_signed_boot(self, log, offset):
        script = f"""
$f = [IO.File]::Open({self.ps(log)}, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::ReadWrite)
try {{
  if ($f.Length -lt {offset}) {{ throw 'compositor log rotated during measurement' }}
  [void]$f.Seek({offset}, [IO.SeekOrigin]::Begin)
  $reader = New-Object IO.StreamReader($f)
  try {{ Write-Output $reader.ReadToEnd() }} finally {{ $reader.Dispose() }}
}} finally {{ $f.Dispose() }}
"""
        text = self.guest(script)
        signed_identity(text, self.c['guest_user'])
        return text

    def check_compositor(self):
        c = self.c
        script = f"(Get-FileHash -Algorithm SHA256 -LiteralPath {self.ps(c['guest_compositor'])}).Hash"
        require(self.guest(script).strip().lower() == c['compositor_sha256'], 'guest compositor hash differs')
        hashes = self.guest("Get-CimInstance Win32_Process -Filter \"Name='sv-guest.exe'\" | ForEach-Object { if (-not $_.ExecutablePath) {throw 'cannot inspect compositor image'}; (Get-FileHash -Algorithm SHA256 -LiteralPath $_.ExecutablePath).Hash }")
        require(all(x.lower() == c['compositor_sha256'] for x in hashes.split()),
                'an existing shared compositor uses an unpinned build')

    def launch(self, args, extra=''):
        require(not self.failed.is_set(), 'health watchdog failed')
        exe, _ = self.paths()
        argv = ','.join(self.ps(x) for x in ['--session-name', self.c['install'], *args])
        return self.guest(extra + f"\n$p=Start-Process -FilePath {self.ps(exe)} -ArgumentList @({argv}) -WorkingDirectory {self.ps(self.c['guest_dir'])} -PassThru; Write-Output $p.Id")

    def stop(self):
        exe, _ = self.paths()
        self.guest(f"& {self.ps(exe)} --session-name {self.ps(self.c['install'])} --stop; if ($LASTEXITCODE -ne 0) {{throw 'stop failed'}}")
        limit = time.monotonic() + 30
        pattern = shlex.quote('wc-launch-' + self.c['install'] + '-*')
        script = ('runuser -u wctest -- env XDG_RUNTIME_DIR=/run/user/$(id -u wctest) '
                  'systemctl --user list-units --all --plain --no-legend ' + pattern + '\n')
        while time.monotonic() < limit:
            if terminal_units(self.host(script)):
                return
            time.sleep(.5)
        raise Refused('scoped stop did not end its host units; no completed batch recorded')

    def read_log(self):
        path = self.ps(self.paths()[1])
        return self.guest(f'if (Test-Path -LiteralPath {path}) {{ Get-Content -Raw -LiteralPath {path} }}')

    def wait_log(self, pattern, previous=0, timeout=130):
        start = time.monotonic()
        while time.monotonic() - start < timeout:
            require(not self.failed.is_set(), 'health watchdog failed')
            text = self.read_log()
            matches = re.findall(pattern, text)
            if len(matches) > previous:
                return text, matches[-1]
            self.done.wait(1)
        raise Refused('launcher completion stamp missing; no retry')

    def live_identity(self, app=False):
        c = self.c
        # Check actual running proxy identity, not just configured mapping.
        script = "ps -eo user=,pid=,args=\n"
        rows = self.host(script).splitlines()
        proxies = [x.split(None, 2) for x in rows if 'wc-proxy ' in x and c['install'] + '-' in x]
        require(proxies and all(x[0] == c['linux_user'] for x in proxies), 'live proxy has wrong mapped user')
        for proxy in proxies:
            require(proxy[1].isdigit(), 'invalid proxy PID')
            digest = self.host(f'sha256sum /proc/{proxy[1]}/exe\n').split()[0]
            require(digest == c['host_binaries'][c['host_proxy']], 'live proxy uses an unpinned binary')
        self.check_compositor()
        if app:
            # The dedicated wctest account must have exactly one stock foot.
            command = f"pgrep -u wctest -f -- {shlex.quote('^' + re.escape(c['app_exec']) + '($| )')}\n"
            pids = self.host(command).split()
            require(len(pids) == 1 and pids[0].isdigit(), 'expected exactly one stock foot process')
            env = self.host(f"tr '\\0' '\\n' </proc/{pids[0]}/environ\n").splitlines()
            for key, suffix in [('HOME', 'home'), ('XDG_CONFIG_HOME', 'config'), ('XDG_DATA_HOME', 'data'), ('XDG_STATE_HOME', 'state')]:
                require(f'{key}={c["test_home"]}/{suffix}' in env, f'live app has wrong {key}')

    def watch(self):
        while not self.done.wait(30):
            try:
                self.health()
            except Exception:
                self.failed.set()
                return

    def run(self, mode, count, minutes, out):
        c = self.c
        require(minutes >= 5, 'lease budget must allow startup and cleanup')
        require(mode != 'batch' or count <= 750, 'split batches at 750 samples or fewer to fit the checked linger')
        require(mode != 'batch' or count <= (minutes * 60 - 150), 'batch exceeds lease budget including setup/cleanup')
        Path(out).mkdir(parents=True, exist_ok=False)
        with (Path(out) / 'commands.jsonl').open('w') as self.log:
            self.preflight()
            name = c['install'] + '-idle-measure-' + str(os.getpid())
            self.command([c['lease_helper'], 'take', name, str(minutes)], timeout=90)
            owned = False
            watch = None
            try:
                self.preflight()  # Recheck after acquiring the guest lease.
                watch = threading.Thread(target=self.watch, daemon=True)
                watch.start()
                _, log = self.paths()
                self.guest(f"if (Test-Path -LiteralPath {self.ps(log)}) {{ Remove-Item -LiteralPath {self.ps(log)} }}")
                comp_log, comp_offset = self.compositor_log_offset()
                owned = True
                if mode == 'batch':
                    self.launch(['--session'])
                    self.wait_log(r'cold start|session up')
                    self.live_identity()
                    trace = c['guest_dir'].rstrip('\\/') + '\\idle-' + Path(out).name + '.txt'
                    require(self.guest(f"Test-Path -LiteralPath {self.ps(trace)}").strip() == 'False', 'trace already exists')
                    exe, _ = self.paths()
                    ps = f"$env:WC_HOP_TRACE={self.ps(trace)}; $env:WC_PING_N='{count}'; $env:WC_PING_GAP_MS='1000'; $env:WC_PING_VERB='rtt'; & {self.ps(exe)} --session-name {self.ps(c['install'])} --ping; if ($LASTEXITCODE -ne 0) {{throw 'ping failed'}}"
                    wall = time.monotonic()
                    self.guest(ps, timeout=count + 90)
                    text = self.guest('Get-Content -Raw -LiteralPath ' + self.ps(trace))
                    samples = parse_samples(text, count)
                    require(time.monotonic() - wall >= count * .95, 'wall clock disagrees with requested idle gaps')
                    (Path(out) / 'L-ping.txt').write_text(text)
                    result = {'kind': mode, 'count': count, 'samples_ns': samples,
                              'source_commit': c['source_commit'], 'guest_sha256': c['guest_sha256']}
                else:
                    # Session-cold and session-warm clicks. The shared compositor
                    # may already exist; never label this a process-cold result.
                    wall = time.monotonic()
                    self.launch(['foot'])
                    text, ms = self.wait_log(r'CLICK-TO-WINDOW (\d+) ms')
                    require(0 < int(ms) <= (time.monotonic() - wall + 1) * 1000, 'invalid guest click clock')
                    self.live_identity(app=True)
                    before = len(re.findall(r'CLICK-TO-WINDOW (\d+) ms', text))
                    wall = time.monotonic()
                    self.launch(['foot'])
                    _, warm = self.wait_log(r'CLICK-TO-WINDOW (\d+) ms', before)
                    require(0 < int(warm) <= (time.monotonic() - wall + 1) * 1000, 'invalid warm clock')
                    result = {'kind': mode, 'session_cold_ms': int(ms), 'session_warm_ms': int(warm)}
                text = self.read_log()
                require(not re.search(r'SSH FALLBACK|DEBUG SSH TRANSPORT|NO DOORBELL', text), 'transport/doorbell fallback in measured session')
                require('shared memory only' in text or 'transport rings:' in text,
                        'launcher log does not confirm shared-memory transport')
                (Path(out) / 'launcher.log').write_text(text)
                (Path(out) / 'broker-signing.log').write_text(self.verify_signed_boot(comp_log, comp_offset))
                self.check_broker_key()
                self.health()
            finally:
                self.done.set()
                if watch:
                    watch.join(timeout=125)
                try:
                    if owned and not self.failed.is_set():
                        self.stop()  # Only this dedicated session, no process kills.
                        self.health()
                finally:
                    self.command([c['lease_helper'], 'release', name])
            require(not self.failed.is_set(), 'health failed; no completed batch recorded')
            require(self.manifest is not None, 'missing immutable measurement manifest')
            result['manifest'] = self.manifest
            result['manifest_sha256'] = fingerprint(self.manifest)
            (Path(out) / 'result.json').write_text(json.dumps(result, indent=2) + '\n')


def report(root, minimum):
    require(minimum >= 2000, 'acceptance requires at least 2000 samples; --minimum cannot lower that gate')
    values = []
    identities = set()
    for file in sorted(Path(root).glob('*/result.json')):
        result = json.loads(file.read_text())
        if result.get('kind') != 'batch':
            continue
        manifest = result.get('manifest')
        require(isinstance(manifest, dict) and manifest.get('schema') == 1, f'missing measurement manifest: {file}')
        digest = fingerprint(manifest)
        require(result.get('manifest_sha256') == digest, f'manifest fingerprint mismatch: {file}')
        identities.add(digest)
        pairs = result['samples_ns']
        require(len(pairs) == result['count'], f'partial batch: {file}')
        values.extend((b - a) / 1000 for a, b in pairs)
    require(len(identities) == 1, 'no samples or mixed measurement manifests; report each configuration separately')
    require(len(values) >= minimum, f'only {len(values)} samples, need {minimum}; keep partial batches explicit')
    values.sort()
    mean = statistics.mean(values)
    return {'count': len(values), 'mean_us': mean, 'p50_us': statistics.median(values),
            'p99_us': values[round((len(values) - 1) * .99)], 'max_us': values[-1],
            'target_mean_below_200us': mean < 200}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('config')
    parser.add_argument('mode', choices=['preflight', 'batch', 'cold', 'report'])
    parser.add_argument('--count', type=int, default=500)
    parser.add_argument('--lease-minutes', type=int, default=12)
    parser.add_argument('--tag')
    parser.add_argument('--minimum', type=int, default=2000)
    args = parser.parse_args()
    c = json.loads(Path(args.config).read_text())
    validate(c)
    require(args.count > 0 and args.minimum > 0 and args.lease_minutes > 0, 'counts/budget must be positive')
    if args.mode == 'report':
        print(json.dumps(report(c['evidence'], args.minimum), indent=2))
    elif args.mode == 'preflight':
        Harness(c).preflight()
        print('system-layout preflight passed; no session started')
    else:
        require(args.tag and re.fullmatch('[A-Za-z0-9_-]+', args.tag), 'a unique --tag is required')
        Harness(c).run(args.mode, args.count, args.lease_minutes, Path(c['evidence']) / args.tag)


if __name__ == '__main__':
    try:
        main()
    except (Refused, OSError, ValueError) as error:
        sys.exit(str(error))
