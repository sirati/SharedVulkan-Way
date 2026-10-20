#!/usr/bin/env python3
"""Cross-build a genuine Cygwin libwayland client and unchanged weston-info.

Invoke through wc-cygwin-build.sh, which owns the shared build cgroup/lock.
This is client-library work, not a rendered/cross-VM acceptance test.
"""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import struct
import subprocess

REPO = Path(__file__).resolve().parent.parent
INTEGRATION = REPO / 'integrations/cygwin'


def run(argv):
    print('+', ' '.join(map(str, argv)), flush=True)
    subprocess.run(list(map(str, argv)), check=True)


def digest(file, algorithm='sha256'):
    with file.open('rb') as stream:
        return hashlib.file_digest(stream, algorithm).hexdigest()


def pe_imports(path):
    """Read the PE import names without executing any target binary."""
    data = path.read_bytes()
    if data[:2] != b'MZ':
        raise RuntimeError(f'not a PE image: {path}')
    pe = struct.unpack_from('<I', data, 0x3c)[0]
    if data[pe:pe + 4] != b'PE\0\0':
        raise RuntimeError(f'invalid PE signature: {path}')
    machine, count = struct.unpack_from('<HH', data, pe + 4)
    optional_size = struct.unpack_from('<H', data, pe + 20)[0]
    optional = pe + 24
    if machine != 0x8664 or struct.unpack_from('<H', data, optional)[0] != 0x20b:
        raise RuntimeError('expected an x86-64 PE32+ image')
    imports_rva = struct.unpack_from('<I', data, optional + 120)[0]
    sections = []
    for i in range(count):
        section = optional + optional_size + i * 40
        virtual_size, address, raw_size, raw = struct.unpack_from('<IIII', data, section + 8)
        sections.append((address, max(virtual_size, raw_size), raw))
    def offset(rva):
        for address, size, raw in sections:
            if address <= rva < address + size:
                return raw + rva - address
        raise RuntimeError(f'PE RVA outside image: {rva}')
    names = []
    if imports_rva:
        position = offset(imports_rva)
        while any(data[position:position + 20]):
            name_rva = struct.unpack_from('<I', data, position + 12)[0]
            start = offset(name_rva)
            end = data.index(b'\0', start)
            names.append(data[start:end].decode('ascii'))
            position += 20
    return names


def fetch(url, target, expected, algorithm):
    if not target.exists():
        part = target.with_suffix(target.suffix + '.partial')
        run(['curl', '-fL', '--connect-timeout', '15', '--max-time', '180', '-o', part, url])
        part.rename(target)
    if digest(target, algorithm) != expected:
        raise RuntimeError(f'checksum mismatch: {target}')


def build(root, bridge_dir=None):
    root = root.resolve()
    if not str(root).startswith('/mnt/datasets/'):
        raise RuntimeError('scratch must be under /mnt/datasets')
    root.mkdir(parents=True, exist_ok=True)
    inside = subprocess.run(['git', '-C', str(root), 'rev-parse', '--is-inside-work-tree'], capture_output=True, text=True)
    if inside.returncode == 0 and inside.stdout.strip() == 'true':
        raise RuntimeError('scratch and artifacts must be outside git worktrees')
    packages = json.loads((INTEGRATION / 'packages.json').read_text())
    sysroot = root / 'sysroot'
    sysroot.mkdir(exist_ok=True)
    for package in packages.values():
        archive = root / Path(package['path']).name
        fetch('https://mirrors.kernel.org/sourceware/cygwin/' + package['path'],
              archive, package['sha512'], 'sha512')
        run(['tar', '-xf', archive, '-C', sysroot])
    sources = json.loads((INTEGRATION / 'sources.json').read_text())
    for name, source in sources.items():
        fetch(source['url'], root / name, source['sha256'], 'sha256')
    run(['tar', '-xf', root / 'wayland-1.24.0.tar.xz', '-C', root])
    source = root / 'wayland-1.24.0'
    output = root / 'client-build'
    output.mkdir(exist_ok=True)
    (source / 'config.h').write_text('#define HAVE_ACCEPT4 1\n#define HAVE_BROKEN_MSG_CMSG_CLOEXEC 1\n#define PACKAGE_VERSION "1.24.0"\n')
    shutil.copy(source / 'config.h', output / 'config.h')
    version = (source / 'src/wayland-version.h.in').read_text()
    for key, value in [('WAYLAND_VERSION', '1.24.0'), ('WAYLAND_VERSION_MAJOR', '1'),
                       ('WAYLAND_VERSION_MINOR', '24'), ('WAYLAND_VERSION_MICRO', '0')]:
        version = version.replace('@' + key + '@', value)
    (output / 'wayland-version.h').write_text(version)
    for mode, extra, name in [('client-header', [], 'wayland-client-protocol.h'),
                              ('client-header', ['-c'], 'wayland-client-protocol-core.h'),
                              ('public-code', [], 'wayland-protocol.c')]:
        run(['wayland-scanner', mode, *extra, source / 'protocol/wayland.xml', output / name])
    gcc = sysroot / 'usr/lib/gcc/x86_64-pc-cygwin/14'
    cc = ['clang', '--target=x86_64-pc-cygwin', '--sysroot=' + str(sysroot),
          '-isystem', str(sysroot / 'usr/include'), '-B' + str(gcc) + '/',
          '-I' + str(output), '-I' + str(source / 'src'),
          '-I' + str(sysroot / 'usr/lib/libffi-3.8.0/include')]
    def compile(file, target, extra=()):
        run([*cc, '-O2', '-c', file, *extra, '-o', target])

    def link(objects, target, shared=False, libraries=()):
        # Clang's Cygwin driver delegates its link to a target GCC executable.
        # Use native lld directly with the official Cygwin GCC CRT/link spec.
        flags = ['--shared', '--enable-auto-image-base', '-e', '_cygwin_dll_entry',
                 '--export-all-symbols', '--out-implib=' + str(output / 'libwayland-client.dll.a')] if shared else ['--subsystem', 'console', '--tsaware']
        startup = [gcc / ('crtbeginS.o' if shared else 'crtbegin.o')]
        if not shared:
            startup.insert(0, sysroot / 'usr/lib/crt0.o')
        run(['ld.lld', '-m', 'i386pep', *flags, '--dll-search-prefix=cyg', '-o', target,
             *startup, *objects, '-L' + str(sysroot / 'usr/lib'),
             '-L' + str(sysroot / 'usr/lib/w32api'), '-L' + str(gcc),
             '-L' + str(output), *libraries, '-lcygwin', '-lgcc_s', '-lgcc',
             '-ladvapi32', '-lshell32', '-luser32', '-lkernel32', gcc / 'crtend.o'])

    # Compile-time ABI proof catches accidentally selecting the MinGW LLP64
    # ABI. Cygwin uses LP64, while both platforms use 64-bit pointers.
    probe = output / 'abi.c'
    probe.write_text('#ifndef __CYGWIN__\n#error not Cygwin\n#endif\n'
                     '_Static_assert(sizeof(long)==8,"Cygwin must be LP64");\n'
                     '#include <stdio.h>\nint main(void){puts("Cygwin LP64 runtime");return 0;}\n')
    compile(probe, output / 'abi.o')
    link([output / 'abi.o'], output / 'abi.exe')
    compile(INTEGRATION / 'fd-probe.c', output / 'fd-probe.o')
    link([output / 'fd-probe.o'], output / 'fd-probe.exe')
    bridge = bridge_dir.resolve() if bridge_dir is not None else None
    if bridge is not None:
        if not (bridge / 'wc_cygwin_link.dll').is_file() or not (bridge / 'libwc_cygwin_link.dll.a').is_file():
            raise RuntimeError('build native wc-cygwin-link for x86_64-pc-windows-gnu first')
        # Integration change is confined to libwayland, never the application.
        client_file = source / 'src/wayland-client.c'
        client = client_file.read_text()
        needle = 'fd = connect_to_socket(name);'
        if client.count(needle) != 1:
            raise RuntimeError('upstream libwayland connect hook changed')
        client_file.write_text('extern int wc_cygwin_ring_socket(void);\n' + client.replace(
            needle, 'fd = getenv("WC_WAYLAND_RING") && strcmp(getenv("WC_WAYLAND_RING"), "1") == 0 ? wc_cygwin_ring_socket() : connect_to_socket(name);'))
    objects = []
    for name in ('wayland-util', 'wayland-client', 'connection'):
        target = output / (name + '.o')
        flags = ['-Dsendmsg=wc_cygwin_sendmsg'] if name == 'connection' else []
        compile(source / ('src/' + name + '.c'), target, flags)
        objects.append(target)
    for file, name in [(output / 'wayland-protocol.c', 'protocol'),
                        (INTEGRATION / 'wayland-os.c', 'os')]:
        target = output / (name + '.o')
        compile(file, target)
        objects.append(target)
    if bridge is not None:
        compile(INTEGRATION / 'ring-transport.c', output / 'ring-transport.o')
        objects.extend([output / 'ring-transport.o', bridge / 'libwc_cygwin_link.dll.a'])
    dll = output / 'cygwayland-client-0.dll'
    link(objects, dll, shared=True, libraries=['-lffi'])
    upstream = root / 'weston-upstream'
    (upstream / 'clients').mkdir(parents=True, exist_ok=True)
    (upstream / 'shared').mkdir(exist_ok=True)
    shutil.copy(root / 'weston-info.c', upstream / 'clients/weston-info.c')
    shutil.copy(root / 'os-compatibility.h', upstream / 'shared/os-compatibility.h')
    app = output / 'weston-info.exe'
    compile(upstream / 'clients/weston-info.c', output / 'weston-info.o')
    link([output / 'weston-info.o'], app, libraries=['-lwayland-client'])
    if digest(upstream / 'clients/weston-info.c') != sources['weston-info.c']['sha256']:
        raise RuntimeError('application source was modified')
    bundle = root / 'bundle'
    bundle.mkdir(exist_ok=True)
    artifacts = [app, dll, output / 'abi.exe', output / 'fd-probe.exe']
    if bridge is not None:
        artifacts.append(bridge / 'wc_cygwin_link.dll')
    helper = sysroot / 'usr/bin/cygwin-console-helper.exe'
    if helper.exists():
        artifacts.append(helper)
    # Include only imported Cygwin DLLs, not target compiler/plugin DLLs.
    available = {p.name.lower(): p for p in (sysroot / 'usr/bin').glob('*.dll')}
    visited = set()
    for image in artifacts:
        for name in pe_imports(image):
            key = name.lower()
            if key in available and key not in visited:
                visited.add(key)
                artifacts.append(available[key])
    if 'cygwin1.dll' not in {name.lower() for name in pe_imports(app)}:
        raise RuntimeError('application does not import the genuine Cygwin runtime')
    manifest = {'packages': packages, 'sources': sources, 'application_source_unmodified': True,
                'integration_files': {p.name: digest(p) for p in INTEGRATION.glob('*.c')},
                'tool_versions': {tool: subprocess.check_output([tool, '--version'], stderr=subprocess.STDOUT, text=True).strip() for tool in ('clang', 'ld.lld', 'wayland-scanner')},
                'transport': ('broker-lent BAR rings and doorbell, descriptors explicitly unsupported'
                              if bridge is not None else 'Cygwin AF_LOCAL, descriptors explicitly unsupported'),
                'artifacts': {}}
    if bridge is not None:
        manifest['native_bridge_source'] = {
            str(file.relative_to(REPO)): digest(file)
            for file in (REPO / 'crates/wc-cygwin-link/src/lib.rs',
                         REPO / 'crates/wc-cygwin-link/Cargo.toml')
        }
    for file in artifacts:
        shutil.copy(file, bundle / file.name)
        manifest['artifacts'][file.name] = {'sha256': digest(file), 'imports': pe_imports(file)}
    (bundle / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
    print('Built Cygwin client bundle:', bundle)
    print('No rendered-frame or cross-VM claim; SCM_RIGHTS/BAR adapter remains required.')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--scratch', type=Path, required=True)
    parser.add_argument('--bridge-dir', type=Path)
    args = parser.parse_args()
    build(args.scratch, args.bridge_dir)
