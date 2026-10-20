#!/usr/bin/env python3
"""Record realized Darwin runtime inputs; a checkout commit is not a receipt."""
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def receipt(path):
    if path.endswith('.drv'):
        derivation = path
    else:
        derivation = subprocess.check_output(['nix-store', '-q', '--deriver', path], text=True).strip()
    value = json.loads(subprocess.check_output(['nix', 'derivation', 'show', derivation], text=True))
    derivations = value.get('derivations', value)
    item = next(iter(derivations.values()))
    environment = item['env']
    patches = environment.get('patches', '').split()
    copied = re.findall(r'\bcp (/nix/store/[^\s]+) src/([^\s]+)', environment.get('postPatch', ''))
    output = environment.get('out', path if not path.endswith('.drv') else None)
    binary = Path(output) / 'bin/hl' if output else None
    forbidden = [p for p in patches if 'test-injection' in p or b'HL_TEST_DEFERRED_EL1' in Path(p).read_bytes()]
    return {
        'derivation': derivation,
        'output': output,
        'binary_sha256': sha(binary) if binary and binary.is_file() else None,
        'upstream_source': environment.get('src'),
        'patches': [{'path': p, 'sha256': sha(p)} for p in patches],
        'copied_sources': [{'path': p, 'target': target, 'sha256': sha(p)} for p, target in copied],
        'build_phase_sha256': hashlib.sha256(environment.get('buildPhase', '').encode()).hexdigest(),
        'excluded_test_injection_check_passed': not forbidden,
        'unexpected_test_injection_paths': forbidden,
    }


print(json.dumps([receipt(path) for path in sys.argv[1:]], indent=2, sort_keys=True))
