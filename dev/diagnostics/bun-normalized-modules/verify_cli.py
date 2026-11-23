"""Authenticate the measured source profile and verify the instrumented ELF."""
from pathlib import Path
import hashlib
import json
import re
import subprocess
import sys
import zipfile

destination, candidate = map(Path, sys.argv[1:])
lock = json.loads(Path(__file__).with_name("contract.json").read_bytes())
def sha(path):
    with path.open("rb") as fd:
        return hashlib.file_digest(fd,"sha256").hexdigest()

def output(args):
    return subprocess.check_output(args, text=True)

def api(path):
    return json.loads(output(["gh","api", "repos/volarized/rift/"+path]))

run = api(f"actions/runs/{lock['prior_run']}")
artifact = api(f"actions/artifacts/{lock['prior_artifact']}")
assert run["head_sha"] == lock["prior_head"] and run["run_attempt"] == 1 and run["conclusion"] == "success"
assert artifact["workflow_run"]["id"] == run["id"] and artifact["workflow_run"]["head_sha"] == run["head_sha"]
assert artifact["name"] == "bun-normalized-module-evidence" and not artifact["expired"]
assert artifact["digest"] == "sha256:" + lock["prior_sha256"]
archive_path = destination / "measured-evidence.zip"
with archive_path.open("xb") as stream:
    subprocess.run(["gh", "api", f"repos/volarized/rift/actions/artifacts/{artifact['id']}/zip"],
                   stdout=stream, check=True)
assert sha(archive_path) == lock["prior_sha256"]
with zipfile.ZipFile(archive_path) as archive:
    expected = json.loads(archive.read("verified.json"))
    prior = json.loads(archive.read("measurements/passed.json"))
assert prior == {"order": "AB", "oracle": lock["prior_oracle"]}
assert expected["candidate"] == lock["prior_candidate_sha256"]

def profile(path, arm):
    raw=output(["readelf","-h","-d","-V","-p",".comment",str(path)])
    (destination/(arm+".readelf")).write_text(raw)
    needed=sorted(re.findall(r"Shared library: \[([^\]]+)\]",raw))
    abi=sorted(set(re.findall(r"\b(?:GLIBC|GLIBCXX|CXXABI|GCC)_[A-Za-z0-9_.]+",raw)))
    rust=sorted(set(re.findall(r"rustc version [^\n]+",raw)))
    assert "ELF64" in raw and "Advanced Micro Devices X86-64" in raw and needed and abi and rust
    return needed,abi,rust,output([str(path),"--version"]).strip()
observed = list(profile(candidate, "candidate"))
assert observed == expected["profile"], (observed, expected["profile"])
(destination / "verified.json").write_text(json.dumps({
    "prior_run": run["id"], "prior_artifact": artifact["id"], "prior_archive": lock["prior_sha256"],
    "prior_candidate": expected["candidate"], "candidate": sha(candidate), "profile": observed,
    "prior_oracle": prior["oracle"],
}, indent=2) + "\n")
