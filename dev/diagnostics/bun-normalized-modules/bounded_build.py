"""Build the ordinary corpus CLI on the same reserved CPUs as the two runtime arms."""
import os
from pathlib import Path
import signal
import subprocess
import sys
import time

from host_guard import HostGuard

evidence = Path(sys.argv[1])
evidence.mkdir()
guard = HostGuard(evidence)
if len(guard.selected) != 2:
    raise ValueError("the comparison requires two reserved CPUs")
process = subprocess.Popen(
    ["just", "integration-cli"], start_new_session=True,
    env=os.environ | {"CARGO_BUILD_JOBS": "2"},
)
try:
    with (evidence / "resources.jsonl").open("x") as journal:
        while process.poll() is None:
            time.sleep(2)
            guard.check(journal, evidence, "candidate compilation", None)
    if process.returncode:
        raise SystemExit(process.returncode)
finally:
    if process.poll() is None:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait()
