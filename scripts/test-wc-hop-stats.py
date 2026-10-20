#!/usr/bin/env python3
"""Regression checks for the clock conversion used in GPU-wake attribution."""
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

SCRIPTS = Path(__file__).resolve().parent


class ClockAttribution(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        (self.root / "guest").mkdir()
        (self.root / "host").mkdir()
        (self.root / "guest/L-1.txt").write_text(
            "10000000000000 L.send\n10000000200000 L.recv\n"
        )
        self.wakes = self.root / "wakes.txt"
        self.wakes.write_text("9992.1\n")

    def stats(self, script, *args):
        return subprocess.run(
            [sys.executable, str(script), str(self.root), *map(str, args)],
            capture_output=True, text=True,
        )

    def test_drift_changes_wake_overlap(self):
        # Over 10,000 seconds, 100 ppm moves the clocks by one second.
        # The corrected request overlaps the wake interval ending at 9999.1 s.
        shutil.copy(SCRIPTS / "wc-hop-stats.py", self.root)
        clock = {"description": "fixture", "reference_ns": 0,
                 "offset_ns": 0, "slope": 0.0001}
        (self.root / "wc-hop-timeline.py").write_text(
            "print(" + repr(json.dumps(clock)) + ")\n"
        )
        result = self.stats(self.root / "wc-hop-stats.py", self.wakes)
        self.assertEqual(result.returncode, 0, result.stderr)
        hit = next(line for line in result.stdout.splitlines() if "within a GPU" in line)
        self.assertIn("n=    1", hit)

    def test_wakes_without_calibration_fail(self):
        result = self.stats(SCRIPTS / "wc-hop-stats.py", self.wakes)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("cannot classify GPU wakes", result.stderr)
        self.assertNotIn("outside GPU wakes", result.stdout)

    def test_guest_only_rtt_still_works(self):
        result = self.stats(SCRIPTS / "wc-hop-stats.py")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("mean    200.0", result.stdout)

    def test_calibration_candidates_without_host_trace(self):
        # There are enough short requests to attempt a fit, but no P.rx.
        events = []
        for i in range(20):
            t = 1000000 + i * 100000
            events += [f"{t} L.send", f"{t + 20000} L.recv"]
        (self.root / "guest/L-1.txt").write_text("\n".join(events) + "\n")
        (self.root / "guest/C-1.txt").write_text("\n".join(
            event for i in range(20) for event in (
                f"{1000000 + i * 100000 + 1000} C.ringtx1",
                f"{1000000 + i * 100000 + 19000} C.reply",
            )
        ) + "\n")
        result = self.stats(SCRIPTS / "wc-hop-stats.py")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("clock: no offset", result.stdout)


if __name__ == "__main__":
    unittest.main()
