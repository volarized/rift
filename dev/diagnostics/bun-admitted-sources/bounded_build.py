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
commands = [
    ("provider", ["cargo", "test", "--locked", "--profile", "corpus", "-p", "rift-provider", "--lib", "normalization::tests", "--", "--nocapture"]),
    ("captured", ["cargo", "test", "--locked", "--profile", "corpus", "-p", "rift-analysis", "--features", "collector,archive,parallel", "--lib", "semantic::captured::tests", "--", "--nocapture"]),
    ("candidate", ["just", "integration-cli"]),
]
required = {
    "provider": ["admitted_range_moves_and_singular_insert_wake_a_missing_alias_until_restoration", "changed_equality_classes_claimant_collisions_and_additional_providers_rebuild"],
    "captured": ["unchanged_admitted_files_share_placements_and_facts_across_held_revisions", "changed_captured_metadata_changes_identity_without_changing_source_or_history"],
}
for name, command in commands:
    log_path = evidence / (name + ".log")
    with log_path.open("x") as log, (evidence / (name + "-resources.jsonl")).open("x") as journal:
        process = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT, start_new_session=True,
                                   env=os.environ | {"CARGO_BUILD_JOBS": "2", "RUST_TEST_THREADS": "2"})
        try:
            while process.poll() is None:
                time.sleep(2)
                guard.check(journal, evidence, name, None)
            if process.returncode:
                print(log_path.read_text()[-6000:], flush=True)
                raise SystemExit(process.returncode)
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
        text = log_path.read_text()
        for case in required.get(name, []):
            if not any(case + " ... ok" in line for line in text.splitlines()):
                raise RuntimeError("missing passed owner case: " + case)
        print(name + " completed", flush=True)
