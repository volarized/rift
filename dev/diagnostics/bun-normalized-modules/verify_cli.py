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
assert run["head_sha"]==os.environ["BASE"] and run["event"]=="push" and run["conclusion"]=="success"
assert row["name"]=="integration-cli" and row["workflow_run"]["id"]==run["id"] and not row["expired"]
assert row["digest"]=="sha256:"+expected["outer.zip"]
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
