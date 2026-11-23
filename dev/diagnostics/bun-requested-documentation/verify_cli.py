"""Authenticate the official archive and compare the two executables' ELF requirements."""
from pathlib import Path
import hashlib
import json
import os
import re
import shutil
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

run = api("actions/runs/"+str(lock["base_run"]))
row = api("actions/artifacts/"+str(lock["base_artifact"]))
expected = lock["base_sha256"]
assert run["head_sha"] == os.environ["BASE"] and run["event"] == "pull_request"
job = api("actions/jobs/" + str(lock["base_job"]))
assert (job["name"], job["status"], job["conclusion"], job["run_id"], job["run_attempt"], job["head_sha"]) == (
    "integration-cli", "completed", "success", run["id"], 1, os.environ["BASE"],
)
synth = api("git/commits/" + lock["base_synth"])
assert synth["verification"]["verified"] and synth["sha"] == lock["base_synth"]
assert [p["sha"] for p in synth["parents"]] == ["69e4ef16c2220a51467e54b688aafcbffcca702f", os.environ["BASE"]]
assert synth["tree"]["sha"] == output(["git","rev-parse",os.environ["BASE"]+"^{tree}"]).strip()
logs = output(["gh","api","--allow-escape-sequences","repos/volarized/rift/actions/jobs/" + str(job["id"]) + "/logs"])
assert lock["base_synth"] + " -> pull/946/merge" in logs and re.search(r"Z " + lock["base_synth"] + r"\s*$", logs, re.M)
assert "SHA256 digest of uploaded artifact is " + expected["outer.zip"] in logs
assert "Artifact integration-cli successfully finalized. Artifact ID " + str(row["id"]) in logs
assert row["name"] == "integration-cli" and row["workflow_run"]["id"] == run["id"] and not row["expired"]
assert row["workflow_run"]["head_sha"] == os.environ["BASE"] and row["digest"] == "sha256:" + expected["outer.zip"]
(destination / "producer.json").write_text(json.dumps({"job": job["id"], "run": run["id"], "run_conclusion":run["conclusion"], "job_conclusion":job["conclusion"], "head":run["head_sha"], "synthetic":synth["sha"], "tree":synth["tree"]["sha"], "archive":row["id"]}, indent=2)+"\n")
for filename in ["outer.zip","integration-cli.tar.zst","rift"]:
    path=destination/filename
    if filename=="outer.zip":
        with path.open("xb") as fd:
            subprocess.run(["gh","api","repos/volarized/rift/actions/artifacts/"+str(row["id"])+"/zip"],stdout=fd,check=True)
    elif filename=="integration-cli.tar.zst":
        with zipfile.ZipFile(destination/"outer.zip") as archive:
            assert archive.namelist()==[filename]
            with archive.open(filename) as source, path.open("xb") as fd: shutil.copyfileobj(source,fd)
    else:
        tarball=str(destination/"integration-cli.tar.zst")
        assert output(["tar","--zstd","-tf",tarball]).splitlines()==["rift"]
        with path.open("xb") as fd:subprocess.run(["tar","--zstd","-xOf",tarball,"rift"],stdout=fd,check=True)
        path.chmod(0o755)
    assert sha(path)==expected[filename]

def profile(path, arm):
    raw=output(["readelf","-h","-d","-V","-p",".comment",str(path)])
    (destination/(arm+".readelf")).write_text(raw)
    needed=sorted(re.findall(r"Shared library: \[([^\]]+)\]",raw))
    abi=sorted(set(re.findall(r"\b(?:GLIBC|GLIBCXX|CXXABI|GCC)_[A-Za-z0-9_.]+",raw)))
    rust=sorted(set(re.findall(r"rustc version [^\n]+",raw)))
    assert "ELF64" in raw and "Advanced Micro Devices X86-64" in raw and needed and abi and rust
    return needed,abi,rust,output([str(path),"--version"]).strip()
baseline=destination/"rift"
a=profile(baseline,"baseline"); b=profile(candidate,"candidate")
assert a==b, (a,b)
(destination/"verified.json").write_text(json.dumps({"base":expected,"candidate":sha(candidate),"profile":a},indent=2)+"\n")
