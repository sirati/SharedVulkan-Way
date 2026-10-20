#!/usr/bin/env python3
"""Freeze a reviewed private fixture and decode real Wayland/import traces.

This prepares artifacts only. It never launches a VM, GPU service or browser.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import sys


def sha(path):
    digest = hashlib.sha256()
    with open(path, "rb") as stream:
        for block in iter(lambda: stream.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def record(path):
    requested = str(path)
    path = Path(path).resolve(strict=True)
    if not path.is_file():
        raise ValueError(f"not a regular artifact: {path}")
    return {"path": str(path), "requested_path": requested, "sha256": sha(path), "bytes": path.stat().st_size}


def prepare(args):
    source = Path(args.source).resolve(strict=True)
    def git(*argv):
        return subprocess.check_output(["git", "-C", str(source), *argv], text=True).strip()
    commit = git("rev-parse", "HEAD")
    if commit != args.expected_commit:
        raise ValueError("source HEAD does not match the reviewed commit")
    if git("status", "--porcelain"):
        raise ValueError("source worktree is dirty; freeze the actual final snapshot first")
    payload = Path(args.payload).resolve(strict=True)
    text = payload.read_text()
    assignments = {}
    for name in ("FF", "WESTON", "MESA", "DRIVER", "GD", "JQ"):
        matches = re.findall(r"^" + name + r"=(.+)$", text, re.M)
        if len(matches) != 1:
            raise ValueError(f"expected one literal {name} assignment")
        values = shlex.split(matches[0])
        if len(values) != 1 or "$" in values[0] or not values[0].startswith("/nix/store/"):
            raise ValueError(f"{name} must name an immutable package")
        assignments[name] = values[0]
    paths = {"payload": str(payload), "firefox": assignments["FF"],
             "weston": assignments["WESTON"], "geckodriver": assignments["GD"],
             "jq": assignments["JQ"], "strace": args.strace,
             "build_log": args.build_log}
    for kind, manifest in (("egl", Path(assignments["MESA"]) / "share/glvnd/egl_vendor.d/50_mesa.json"),
                           ("icd", Path(assignments["DRIVER"]) / "share/vulkan/icd.d/sv_icd.json")):
        library = json.loads(manifest.read_text())["ICD"]["library_path"]
        if not Path(library).is_absolute():
            raise ValueError(f"{kind} library path is not absolute")
        paths[kind + "_manifest"] = str(manifest)
        paths[kind + "_library"] = library
    for item in args.artifact:
        name, path = item.split("=", 1)
        if not re.fullmatch(r"[a-z0-9_-]+", name) or name in paths:
            raise ValueError("invalid or duplicate artifact name")
        paths[name] = path
    for required in ("sv-drm", "sv-hostd", "wc-broker", "sv-gpu-server", "vk-probe", "kernel", "initrd",
                     "qemu", "boot-script", "setup-script", "gpu-script", "guest-runner"):
        if required not in paths:
            raise ValueError(f"missing exact artifact {required}")
    artifacts = {name: record(path) for name, path in paths.items()}
    bindings = {}
    for item in args.binding:
        name, declared_commit = item.split("=", 1)
        if name not in artifacts or name in bindings:
            raise ValueError("invalid or duplicate source binding")
        if not re.fullmatch(r"[0-9a-fA-F]{7,40}", declared_commit):
            raise ValueError("source bindings must be concrete commit hashes")
        bindings[name] = git("rev-parse", "--verify", declared_commit + "^{commit}")
    for name in ("sv-drm", "sv-hostd", "wc-broker", "sv-gpu-server", "vk-probe", "icd_library"):
        if name not in bindings:
            raise ValueError(f"missing declared source commit for {name}")
    destination = Path(args.output)
    parent = destination.parent.resolve(strict=True)
    inside = subprocess.run(["git", "-C", str(parent), "rev-parse", "--is-inside-work-tree"],
                            capture_output=True, text=True)
    if inside.returncode == 0:
        raise ValueError("artifact snapshots must be outside git")
    destination.mkdir(mode=0o700, parents=False, exist_ok=False)
    assets = destination / "assets"
    assets.mkdir()
    for name in ("sv-drm", "sv-hostd", "wc-broker", "sv-gpu-server", "vk-probe", "build_log",
                 "boot-script", "setup-script", "gpu-script", "guest-runner", "payload"):
        target = assets / name
        subprocess.run(["cp", "--reflink=always", "--", artifacts[name]["path"], str(target)], check=True)
        if sha(target) != artifacts[name]["sha256"]:
            raise ValueError(f"snapshot copy differs for {name}")
        target.chmod(0o555)
        artifacts[name]["snapshot_path"] = str(target.resolve())
    wrapper = Path(__file__).with_name("sv-linux-bar-audit-weston.sh")
    (destination / "weston-audit.sh").write_bytes(wrapper.read_bytes())
    replacement = shlex.quote(args.guest_output.rstrip("/") + "/weston-audit.sh")
    text, count = re.subn(r"^WESTON=.+$", lambda _: "WESTON=" + replacement, text, count=1, flags=re.M)
    if count != 1:
        raise ValueError("could not bind the traced Weston wrapper")
    # Only the compositor is traced. Firefox and its sandbox retain their
    # original process ancestry and environment, except diagnostic logging.
    if text.count("export WAYLAND_DISPLAY=rdd-test\n") != 1:
        raise ValueError("payload lacks the unique audit environment insertion point")
    exports = ("export SV_AUDIT_REAL_WESTON=" + shlex.quote(assignments["WESTON"]) + "\n"
               "export SV_AUDIT_STRACE=" + shlex.quote(artifacts["strace"]["path"]) + "\n")
    text = text.replace("export WAYLAND_DISPLAY=rdd-test\n", exports + "export WAYLAND_DISPLAY=rdd-test\n", 1)
    generated = destination / "firefox-audit-payload.sh"
    generated.write_text(text)
    for path in (generated, destination / "weston-audit.sh"):
        path.chmod(0o555)
        subprocess.run(["bash", "-n", str(path)], check=True)
    receipt = {"schema": 1, "source": str(source), "commit": commit,
               "source_clean": True, "selected_package_paths": assignments, "build_binding": "review the recorded build log against this source commit",
               "artifacts": artifacts, "declared_component_source_commits": bindings,
               "auditor": record(__file__),
               "launch_requirement": "execute copied binaries and verify actual process exe/argv/maps against this receipt before testing",
               "derived_payload": record(generated), "weston_wrapper": record(destination / "weston-audit.sh"),
               "guest_output": args.guest_output,
               "bounds": {"guest_ram_mib": 3072, "vcpus": 2, "vm_seconds": 600,
                          "gpu_seconds": 480, "initial_bar_mib": 256,
                          "weston_trace_file_max_kib": 65536},
               "claims": "metadata instrumentation only; no GPU, decode, latency or sandbox proof"}
    (destination / "audit-receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(destination / "audit-receipt.json")


def verify(args):
    receipt = json.loads(Path(args.receipt).read_text())
    artifacts = list(receipt["artifacts"].values()) + [receipt["derived_payload"], receipt["weston_wrapper"]]
    for artifact in artifacts:
        path = artifact.get("snapshot_path", artifact["path"])
        if sha(path) != artifact["sha256"]:
            raise ValueError(f"artifact changed since snapshot: {path}")
    print("PASS: frozen artifact bytes match the receipt; build/source review is still required")


def number(text):
    text = text.strip()
    if text.startswith("fd "):
        text = text[3:]
    return int(text, 16 if text.lower().startswith("0x") else 10)


def decode_protocol(lines):
    pending, imports, errors = {}, [], []
    pattern = re.compile(r"(?:zwp_)?linux_buffer_params_v1(?:@|#)(\d+)\.(add|create_immed|create|destroy)\((.*)\)")
    for index, line in enumerate(lines, 1):
        match = pattern.search(line)
        if not match:
            continue
        identity, operation, raw = match.groups()
        values = [value.strip() for value in raw.split(",")]
        state = pending.setdefault(identity, {"planes": [], "ambiguous": False})
        if operation == "destroy":
            pending.pop(identity, None)
            continue
        try:
            if operation == "add":
                if len(values) != 6:
                    raise ValueError("plane add does not have six fields")
                fd, plane, offset, stride, high, low = map(number, values)
                if not (0 <= high <= 0xffffffff and 0 <= low <= 0xffffffff):
                    raise ValueError("modifier components exceed u32")
                if any(p["plane"] == plane for p in state["planes"]):
                    state["ambiguous"] = True
                state["planes"].append({"plane": plane, "received_fd": fd, "offset": offset,
                    "stride": stride, "modifier": f"0x{((high << 32) | low):016x}", "line": index})
                continue
            if operation == "create_immed":
                values = values[1:]
            if len(values) != 4:
                raise ValueError("create does not have width, height, format and flags")
            width, height, fmt, flags = map(number, values)
            if not (0 <= fmt <= 0xffffffff):
                raise ValueError("format exceeds u32")
            planes = state["planes"]
            complete = bool(planes) and not state["ambiguous"]
            fourcc = fmt.to_bytes(4, "little").decode("ascii", "replace")
            imports.append({"params_id": identity, "width": width, "height": height,
                "format": f"0x{fmt:08x}", "fourcc": fourcc, "flags": flags, "planes": planes,
                "line": index, "complete_plane_fields_no_observed_collision": complete,
                "correlation": "correlate timestamps and received FDs with strace socket identities; object IDs are client-local"})
            pending.pop(identity, None)
        except ValueError as error:
            errors.append({"line": index, "error": str(error), "raw": line.rstrip()})
            pending.pop(identity, None)
    return {"imports": imports, "parse_errors": errors,
            "requires_native_fd_correlation": True,
            "unconsumed_plane_sets": pending,
            "complete_import_metadata": bool(imports) and not errors and not pending and all(i["complete_plane_fields_no_observed_collision"] for i in imports)}


def decode(args):
    protocol = Path(args.protocol)
    result = decode_protocol(protocol.read_text(errors="replace").splitlines())
    result["protocol_log"] = record(protocol)
    result["native_trace_logs"] = [record(path) for path in args.trace]
    result["native_failures"] = []
    result["received_descriptor_records"] = []
    for path in args.trace:
        for line_number, line in enumerate(Path(path).read_text(errors="replace").splitlines(), 1):
            entry = {"path": str(path), "line": line_number, "raw": line}
            if "SCM_RIGHTS" in line:
                result["received_descriptor_records"].append(entry)
            if re.search(r"\b(ioctl|mmap|recvmsg)\(", line) and re.search(r"= -1 [A-Z_]+", line):
                result["native_failures"].append(entry)
    result["acceptance"] = "diagnostic receipt only; absence of a kernel error does not establish native import success"
    Path(args.output).write_text(json.dumps(result, indent=2) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    freeze = commands.add_parser("prepare")
    for field in ("source", "expected-commit", "payload", "strace", "build-log", "output", "guest-output"):
        freeze.add_argument("--" + field, required=True)
    freeze.add_argument("--artifact", action="append", default=[])
    freeze.add_argument("--binding", action="append", default=[])
    freeze.set_defaults(run=prepare)
    check = commands.add_parser("verify")
    check.add_argument("--receipt", required=True)
    check.set_defaults(run=verify)
    parse = commands.add_parser("decode")
    parse.add_argument("--protocol", required=True)
    parse.add_argument("--trace", action="append", default=[])
    parse.add_argument("--output", required=True)
    parse.set_defaults(run=decode)
    args = parser.parse_args()
    args.run(args)


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"HOLD: {error}", file=sys.stderr)
        sys.exit(2)
