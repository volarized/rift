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
    ("analysis", ["cargo", "test", "--locked", "--profile", "corpus", "-p", "rift-analysis", "--features", "collector,archive,parallel", "--lib", "documentation::", "--", "--nocapture"]),
    ("no-default-lib", ["cargo", "check", "--locked", "--profile", "corpus", "-p", "rift-analysis", "--no-default-features", "--lib"]),
    ("index", ["cargo", "test", "--locked", "--profile", "corpus", "-p", "rift-index", "--lib", "documentation::tests", "--", "--nocapture"]),
    ("candidate", ["just", "integration-cli"]),
]
raw_case = "canonical_documentation_symbol_matches_raw_identity_name_and_range_refusals"
required = {
    "analysis": [raw_case, "requested_names_include_previous_misses_and_restore_cross_language_ambiguity",
                 "encoded_qualified_link_wakes_only_for_its_source_and_keeps_duplicate_claims",
                 "fragment_range_and_duplicate_changes_recompute_link_block_only"],
    "index": ["attached_documentation_matches_accepted_source_and_symbol",
              "named_attachments_preserve_full_documentation_and_wake_missing_names"],
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
