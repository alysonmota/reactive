#!/usr/bin/env python3
"""Fast lifecycle tests; does not build Rust or run the benchmark matrix."""
import importlib.util
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest

spec = importlib.util.spec_from_file_location("ablation", Path(__file__).with_name("protection-ablation.py"))
ablation = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ablation)


class ProcessLifecycle(unittest.TestCase):
    def setUp(self):
        self.previous_timeout = ablation.STEP_TIMEOUT

    def tearDown(self):
        ablation.STEP_TIMEOUT = self.previous_timeout

    def test_success_and_checked_failure(self):
        ablation.STEP_TIMEOUT = 5
        self.assertEqual(ablation.run_step([sys.executable, "-c", "pass"]).returncode, 0)
        with self.assertRaises(subprocess.CalledProcessError):
            ablation.run_step([sys.executable, "-c", "raise SystemExit(7)"], check=True)

    def test_timeout_kills_descendant_even_when_leader_exits_on_term(self):
        ablation.STEP_TIMEOUT = 1
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
            marker = Path(directory) / "child.pid"
            child = "import signal,time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(60)"
            parent = (
                "import subprocess,sys,time; from pathlib import Path; "
                f"p=subprocess.Popen([sys.executable,'-c',{child!r}]); "
                f"Path({str(marker)!r}).write_text(str(p.pid)); time.sleep(60)"
            )
            started = time.monotonic()
            try:
                result = ablation.run_step([sys.executable, "-c", parent])
                self.assertEqual(result.returncode, 124)
                self.assertLess(time.monotonic() - started, 5)
                self.assertTrue(marker.exists())
                pid = int(marker.read_text())
                # A killed descendant may remain a zombie until init reaps it.
                for _ in range(100):
                    status = Path(f"/proc/{pid}/stat")
                    if not status.exists() or status.read_text().split(") ", 1)[1].split()[0] == "Z":
                        break
                    time.sleep(0.01)
                else:
                    self.fail("descendant survived process-group timeout")
            finally:
                if marker.exists():
                    try:
                        os.kill(int(marker.read_text()), signal.SIGKILL)
                    except ProcessLookupError:
                        pass


if __name__ == "__main__":
    unittest.main()
