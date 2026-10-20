#!/usr/bin/env python3
"""Build genuine LLP64 libwayland and unchanged upstream native applications.

Only the shipped library/runtime is ported. Application hashes are checked
before and after compiling. No Windows target executable runs during build.
"""
import argparse
import sys
sys.dont_write_bytecode = True
import importlib.util
import json
from pathlib import Path
import shutil
import subprocess

REPO = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location('cygwin_build', REPO / 'scripts/wc-cygwin-build.py')
common = importlib.util.module_from_spec(spec)
spec.loader.exec_module(common)
run = common.run


def provenance(root, output, source, bundle, manifest):
    inputs = ['flake.lock', 'scripts/wc-local-runtime.ps1', 'scripts/wc-mingw-build.py', 'scripts/wc-cygwin-local-build.py',
              'scripts/wc-cygwin-build.py', 'integrations/cygwin/sources.json',
              'integrations/cygwin/packages.json', 'integrations/cygwin/ring-transport.c',
              'integrations/cygwin/wayland-os.c']
    manifest['build_inputs'] = {name: common.digest(REPO / name) for name in inputs}
    manifest['generated_sources'] = {}
    directory = bundle / 'provenance'
    directory.mkdir(exist_ok=True)
    for name in inputs:
        target = directory / 'inputs' / name
        target.parent.mkdir(parents=True, exist_ok=True)
        if target.exists(): target.chmod(0o600)
        shutil.copyfile(REPO / name, target)
        manifest['generated_sources'][str(target.relative_to(bundle))] = common.digest(target)
    generated = [source / 'src/connection.c', source / 'src/wayland-client.c', output / 'config.h', output / 'abi.c']
    if (output / 'local-transport.c').exists(): generated.append(output / 'local-transport.c')
    for original in generated + [root / 'weston-info.c', root / 'simple-shm.c']:
        target = directory / original.name
        if target.exists(): target.chmod(0o600)
        shutil.copyfile(original, target)
        manifest['generated_sources']['provenance/' + target.name] = common.digest(target)


def patch_descriptors(source):
    # The library wire decoder already knows every signature, including fds.
    # Local Windows peers use canonical variable-size named Section descriptors.
    file = source / 'src/connection.c'
    text = file.read_text()
    at = text.index('{', text.index('copy_fds_to_connection(struct wl_closure *closure,')) + 1
    text = text[:at] + '\n\treturn 0; /* descriptors travel in-band */\n' + text[at:]
    start = text.index('buffer_size_for_closure(struct wl_closure *closure)')
    end = text.index('serialize_closure(struct wl_closure *closure,')
    old = 'case WL_ARG_FD:\n\t\t\tbreak;'
    part = text[start:end]
    if part.count(old) != 1:
        raise RuntimeError('upstream FD sizing changed')
    part = part.replace(old, 'case WL_ARG_FD:\n\t\t\tbuffer_size += wc_native_fd_word_count(closure->args[i].h);\n\t\t\tbreak;')
    text = text[:start] + part + text[end:]
    start = text.index('serialize_closure(struct wl_closure *closure,')
    end = text.index('\nint\nwl_closure_send(', start)
    part = text[start:end].replace('if (arg.type == WL_ARG_FD)\n\t\t\tcontinue;', '')
    if part.count(old) != 1:
        raise RuntimeError('upstream FD serialization changed')
    part = part.replace(old, 'case WL_ARG_FD: {\n\t\t\tsize_t words = wc_native_fd_word_count(closure->args[i].h);\n\t\t\tif (!words || words > (size_t)(end-p)) goto overflow;\n\t\t\tif (wc_native_fd_words(closure->args[i].h,p,words)) return -1;\n\t\t\tp += words; break;\n\t\t}')
    text = text[:start] + part + text[end:]
    start = text.index('wl_connection_demarshal(struct wl_connection *connection,')
    end = text.index('\nint\nwl_closure_lookup_objects(', start)
    part = text[start:end]
    at = part.index('case WL_ARG_FD:') + len('case WL_ARG_FD:')
    part = part[:at] + '''
            if (p == end || !p[0] || p[0] > 4096) { errno=EPROTO; goto err; }
            {
                size_t words = 1 + ((size_t)p[0] + 3) / 4;
                if (words > (size_t)(end-p)) { errno=EPROTO; goto err; }
                fd = wc_native_fd_from_words(p, words);
                if (fd < 0) goto err;
                closure->args[i].h=fd; p += words; break;
            }
''' + part[at:]
    file.write_text(text[:start] + part + text[end:])



def build(root):
    root = root.resolve()
    if not str(root).startswith('/mnt/datasets/'):
        raise RuntimeError('scratch must be under /mnt/datasets')
    root.mkdir(parents=True, exist_ok=True)
    if subprocess.run(['git', '-C', root, 'rev-parse', '--is-inside-work-tree'], capture_output=True).returncode == 0:
        raise RuntimeError('build artifacts must be outside git')
    pins = json.loads((REPO / 'integrations/cygwin/sources.json').read_text())
    for name, item in pins.items():
        common.fetch(item['url'], root / name, item['sha256'], 'sha256')
    source = root / 'wayland-1.24.0'
    if source.exists():
        shutil.rmtree(source)
    run(['tar', '-xf', root / 'wayland-1.24.0.tar.xz', '-C', root])
    output = root / 'build'
    output.mkdir(exist_ok=True)
    paths = subprocess.check_output(['nix', 'build', '--no-link', '--print-out-paths', '.#libffi-mingw', '.#libffi-mingw.dev', '.#pthreads-mingw'], text=True).splitlines()
    ffi_headers = next(path.parent for item in paths for path in Path(item).rglob('ffi.h'))
    flags = ['x86_64-w64-mingw32-gcc', '-O2', '-D_GNU_SOURCE', '-D__USE_MINGW_ANSI_STDIO=1',
             '-I' + str(REPO / 'integrations/mingw/include'), '-I' + str(REPO / 'integrations/windows-local'),
             '-I' + str(output), '-I' + str(source / 'src'), '-I' + str(ffi_headers),
             *['-I' + str(Path(path) / 'include') for path in paths],
             '-include', str(REPO / 'integrations/mingw/include/wc-native.h')]
    def compile(file, name, extra=()):
        obj = output / (name + '.o')
        run([*flags, *extra, '-c', file, '-o', obj])
        return obj
    libraries = [arg for path in paths for arg in ['-L' + str(Path(path) / 'lib')]]
    native = REPO / 'target/x86_64-pc-windows-gnu/debug'
    run(['cargo', 'build', '-p', 'wc-local-client', '--target', 'x86_64-pc-windows-gnu'])
    libraries += ['-L' + str(output), '-L' + str(native)]
    (output / 'config.h').write_text('#define HAVE_ACCEPT4 1\n#define PACKAGE_VERSION "1.24.0"\n')
    shutil.copy(output / 'config.h', source / 'config.h')
    version = (source / 'src/wayland-version.h.in').read_text()
    for name, value in [('WAYLAND_VERSION', '1.24.0'), ('WAYLAND_VERSION_MAJOR', '1'), ('WAYLAND_VERSION_MINOR', '24'), ('WAYLAND_VERSION_MICRO', '0')]:
        version = version.replace('@' + name + '@', value)
    (output / 'wayland-version.h').write_text(version)
    for mode, extra, name in [('client-header', [], 'wayland-client-protocol.h'), ('client-header', ['-c'], 'wayland-client-protocol-core.h'), ('public-code', [], 'wayland-protocol.c')]:
        run(['wayland-scanner', mode, *extra, source / 'protocol/wayland.xml', output / name])

    patch_descriptors(source)
    connection = source / 'src/connection.c'
    text = connection.read_text()
    # UCRT has no open_memstream. Serialize debug output to stderr under the
    # shipped runtime's mutex; no application or pixel path changes.
    text = text.replace('char *buffer;\n\tsize_t buffer_length;', '')
    text = text.replace('f = open_memstream(&buffer, &buffer_length);\n\tif (f == NULL)\n\t\treturn;', 'wc_native_log_lock();\n\tf = stderr;')
    text = text.replace('if (fclose(f) == 0) {\n\t\tfprintf(stderr, "%s", buffer);\n\t\tfree(buffer);\n\t}', 'fflush(f);\n\twc_native_log_unlock();')
    connection.write_text(text)

    runtime = [compile(REPO / 'integrations/mingw/posix.c', 'posix', ['-Wall', '-Wextra', '-Werror'])]
    run(['x86_64-w64-mingw32-gcc', '-shared', '-o', output / 'wc-native-posix.dll', *runtime, *libraries,
         '-lwc_local_client', '-lwinpthread', '-Wl,--export-all-symbols', '-Wl,--out-implib=' + str(output / 'libwc-native-posix.dll.a')])
    objects = [compile(source / ('src/' + name + '.c'), name) for name in ('wayland-util', 'wayland-client', 'connection')]
    objects += [compile(output / 'wayland-protocol.c', 'protocol'), compile(REPO / 'integrations/mingw/wayland-os.c', 'os', ['-Wall', '-Wextra', '-Werror'])]
    run(['x86_64-w64-mingw32-gcc', '-shared', '-o', output / 'wayland-client-0.dll', *objects, *libraries,
         '-lwc-native-posix', '-lffi', '-lwinpthread', '-Wl,--export-all-symbols', '-Wl,--out-implib=' + str(output / 'libwayland-client.dll.a')])
    upstream = root / 'weston-upstream'
    for directory in ['clients', 'shared', 'libweston']:
        (upstream / directory).mkdir(parents=True, exist_ok=True)
    for name, target in [('weston-info.c', 'clients/weston-info.c'), ('os-compatibility.h', 'shared/os-compatibility.h'), ('simple-shm.c', 'clients/simple-shm.c'), ('zalloc.h', 'libweston/zalloc.h')]:
        shutil.copy(root / name, upstream / target)
    protocols = []
    for name in ['xdg-shell', 'fullscreen-shell-unstable-v1']:
        run(['wayland-scanner', 'client-header', root / (name + '.xml'), output / (name + '-client-protocol.h')])
        run(['wayland-scanner', 'public-code', root / (name + '.xml'), output / (name + '-protocol.c')])
        protocols.append(compile(output / (name + '-protocol.c'), name))
    for name, sources in [('weston-info', []), ('simple-shm', protocols)]:
        obj = compile(upstream / ('clients/' + name + '.c'), name, ['-I' + str(upstream)])
        run(['x86_64-w64-mingw32-gcc', '-o', output / (name + '.exe'), obj, *sources, *libraries, '-lwayland-client', '-lwc-native-posix', '-lwinpthread'])
        if common.digest(upstream / ('clients/' + name + '.c')) != pins[name + '.c']['sha256']:
            raise RuntimeError('application source modified')
    obj = compile(REPO / 'integrations/windows-local/posix-probe.c', 'posix-probe')
    run(['x86_64-w64-mingw32-gcc', '-o', output / 'posix-probe.exe', obj, *libraries, '-lwc-native-posix', '-lwc_local_client', '-lwinpthread'])
    abi = output / 'abi.c'
    abi.write_text('#ifdef __CYGWIN__\n#error Cygwin is not MinGW\n#endif\n#include <stdio.h>\n_Static_assert(sizeof(long)==4,"MinGW LLP64");\n_Static_assert(sizeof(void*)==8,"64bit pointers");\nint main(void){puts("MinGW LLP64 runtime");return 0;}\n')
    run(['x86_64-w64-mingw32-gcc', abi, '-o', output / 'abi.exe'])
    bundle = root / 'bundle'
    bundle.mkdir(exist_ok=True)
    artifacts = [output / name for name in ['abi.exe', 'posix-probe.exe', 'weston-info.exe', 'simple-shm.exe', 'wayland-client-0.dll', 'wc-native-posix.dll']]
    artifacts += [native / 'wc_local_client.dll', native / 'wc-local-namespace.exe']
    for path in paths:
        artifacts.extend(Path(path).rglob('*.dll'))
    manifest = {'sources': pins, 'artifacts': {}, 'adapter_sources': {str(file.relative_to(REPO)): common.digest(file) for directory in ['crates/wc-local-client', 'integrations/mingw', 'integrations/windows-local'] for file in (REPO / directory).rglob('*') if file.is_file()}}
    for artifact in artifacts:
        destination = bundle / artifact.name
        if destination.exists(): destination.chmod(0o600)
        shutil.copyfile(artifact, destination)
        imports = common.pe_imports(artifact)
        if any('cygwin' in item.lower() for item in imports):
            raise RuntimeError('native binary depends on Cygwin')
        manifest['artifacts'][artifact.name] = {'sha256': common.digest(artifact), 'imports': imports}
    provenance(root, output, source, bundle, manifest)
    (bundle / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
    print('MINGW_ORDINARY_APP_BUILD_OK', bundle, flush=True)


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--scratch', required=True, type=Path)
    build(parser.parse_args().scratch)
