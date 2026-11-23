"""Verify and extract two plain binaries from their exact ordinary CI artifacts."""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
from zipfile import ZipFile

MAX_ARCHIVE = 512 * 1024**2
MAX_BINARY = 1024 * 1024**2
REPO = "volarized/rift"
SOURCES = ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml", ".cargo/config.toml", "justfile", "dev/uv.lock",
           "dev/pyproject.toml", "dev/src/rift_dev/build_run.py", "dev/src/rift_dev/build_cache.py")
WORKFLOW = ".github/workflows/ci.yml"
ESCAPES = re.compile(rb"\x1b\[[0-?]*[ -/]*[@-~]")
BUILD = "uv run --locked --project dev rift-dev build --locked --profile corpus -p rift"


def require(condition, explanation):
    if not condition:
        raise ValueError(explanation)


def sha(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def api(path):
    return json.loads(subprocess.check_output(["gh", "api", f"repos/{REPO}/{path}"], timeout=20))


def check_contract(contract, armed):
    require(contract["repository"] == REPO and contract["profile"] == "corpus"
            and contract["platform"] == "ubuntu-24.04/x86_64", "unexpected benchmark recipe")
    base, candidate = contract["base"], contract["candidate"]
    require(candidate["parents"] == [contract["candidate_base"], candidate["workflow_head"]],
            "candidate synthetic parents differ from the paired source commits")
    require(contract["bun_pin"] == "744846f844374847c902b5e7fd59b4342a51ef99", "wrong Bun pin")
    for label, row in (("base", base), ("candidate", candidate)):
        require(row["event"] == {"base": "push", "candidate": "pull_request"}[label], "wrong ordinary CI event")
        for field in ("workflow_head", "built_commit", "tree"):
            require(re.fullmatch(r"[0-9a-f]{40}", row[field]), f"{label} malformed {field}")
        if not armed and label == "candidate":
            continue
        for field in ("run", "job", "artifact"):
            require(type(row[field]) is int and row[field] > 0, f"{label} is missing exact {field}")
        for field in ("archive_sha256", "zst_sha256", "binary_sha256"):
            digest = row[field]
            require(isinstance(digest, str) and re.fullmatch(r"[0-9a-f]{64}", digest),
                    f"{label} has no exact {field}")


def verify_run(row, destination):
    run = api(f"actions/runs/{row['run']}")
    require(run["id"] == row["run"] and run["head_sha"] == row["workflow_head"]
            and run["run_attempt"] == 1 and run["event"] == row["event"]
            and run["path"] == ".github/workflows/ci.yml", "run is not the exact ordinary CI invocation")
    job = api(f"actions/jobs/{row['job']}")
    require(job["id"] == row["job"] and job["run_id"] == row["run"] and job["name"] == "integration-cli"
            and job["labels"] == ["ubuntu-24.04"] and job["conclusion"] == "success",
            "ordinary integration-cli job does not match")
    built = [step for step in job["steps"] if step["name"] == "Build and archive the CLI"]
    require(len(built) == 1 and built[0]["conclusion"] == "success",
            "the exact ordinary CLI must have been built rather than restored as an older binary")
    origin = api(f"git/commits/{row['built_commit']}")
    require(origin["sha"] == row["built_commit"] and origin["tree"]["sha"] == row["tree"],
            "built commit tree differs")
    signed = api(f"commits/{row['workflow_head']}")
    require(signed["commit"]["verification"]["verified"] is True
            and signed["commit"]["tree"]["sha"] == row["tree"], "unsigned or different product tree")
    if row["event"] == "pull_request":
        require([parent["sha"] for parent in origin["parents"]] == row["parents"],
                "pull-request synthetic parents changed")
    artifact = api(f"actions/artifacts/{row['artifact']}")
    require(artifact["id"] == row["artifact"] and artifact["name"] == "integration-cli"
            and artifact["expired"] is False and 0 < artifact["size_in_bytes"] <= MAX_ARCHIVE
            and artifact["digest"] == "sha256:" + row["archive_sha256"]
            and artifact["workflow_run"]["id"] == row["run"]
            and artifact["workflow_run"]["head_sha"] == row["workflow_head"], "artifact origin differs")
    for name, data in (("run", run), ("job", job), ("synthetic", origin), ("signed", signed), ("artifact", artifact)):
        (destination / f"{name}.json").write_text(json.dumps(data, indent=2) + "\n")
    return artifact


def source_build_inputs(row):
    return {path: api(f"contents/{path}?ref={row['built_commit']}")["sha"] for path in SOURCES}


def qualify_source_workflow(contract, destination):
    base, candidate = contract["base"], contract["candidate"]
    source = api(f"commits/{contract['candidate_base']}")
    require(source["commit"]["verification"]["verified"] is True
            and [parent["sha"] for parent in source["parents"]] == [base["built_commit"]]
            and {item["filename"] for item in source["files"]} == {WORKFLOW, ".github/workflows/rift-release.yml"},
            "candidate parent is no longer the reviewed workflow-only successor")
    workflows = {label: api(f"contents/{WORKFLOW}?ref={row['built_commit']}")["sha"]
                 for label, row in (("base", base), ("candidate", candidate))}
    require(workflows == contract["workflow_blobs"], "actual ordinary workflow differs from its reviewed source")
    (destination / "workflow-inputs.json").write_text(json.dumps(workflows, indent=2) + "\n")


def actual_profile(row, destination):
    log = subprocess.check_output(["gh", "api", "--allow-escape-sequences",
                                   f"repos/{REPO}/actions/jobs/{row['job']}/logs"], timeout=25)
    require(len(log) < 4 * 1024**2, "ordinary build log exceeded its evidence bound")
    clean = ESCAPES.sub(b"", log).decode("utf-8")
    image = re.search(r"Image: ubuntu-24\.04\s+\S+ Version: ([\d.]+)", clean)
    stamps = {name: re.findall(pattern, clean) for name, pattern in {
        "uv": r"Successfully installed uv version ([\d.]+)",
        "just": r"(?m)^\S+ just ([\d.]+)$",
        "sccache": r"(?m)^\S+ sccache ([\d.]+)$",
        "python": r"Using CPython ([\d.]+) interpreter",
    }.items()}
    require(image is not None and all(len(values) == 1 for values in stamps.values()),
            "ordinary job lacks exact tool or host-image evidence")
    require(re.search(rf"(?m)^\S+ {row['built_commit']}$", clean) is not None,
            "ordinary job log lacks independently checked-out source")
    require(BUILD in clean and "RUSTC_WRAPPER: sccache" in clean and "CARGO_INCREMENTAL: 0" in clean,
            "locked ordinary corpus recipe or compile settings differ")
    elf = subprocess.check_output(["readelf", "--wide", "-p", ".comment", "-d", "-V",
                                   str(destination / "rift")], text=True, timeout=15)
    tools = re.findall(r"(?m)^\[\s*[0-9a-f]+\][ \t]+(.*)$", elf)
    needed = re.findall(r"\(NEEDED\)\s+Shared library: \[([^\]]+)\]", elf)
    abi = sorted(set(re.findall(r"\bName: ((?:OPENSSL|GLIBC|GCC)_[0-9.]+)", elf)))
    require(any(value.startswith("rustc version 1.98.1 (48a229cea ") for value in tools)
            and any(value.startswith("Linker: LLD 22.1.8 ") for value in tools)
            and bool(needed) and bool(abi), "compiler, linker or runtime ELF information differs")
    result = {"image": image.group(1), "tools": stamps, "compiler_linker": tools,
              "linked_libraries": needed, "abi_versions": abi, "build": BUILD}
    (destination / "actual-profile.json").write_text(json.dumps(result, indent=2) + "\n")
    return result


def download(row, archive):
    with archive.open("xb") as output:
        subprocess.run(["gh", "api", "--allow-escape-sequences", f"repos/{REPO}/actions/artifacts/{row['artifact']}/zip"],
                       stdout=output, check=True, timeout=92)


def extract(row, destination, expected_size):
    archive = destination / "ordinary.zip"
    require(archive.stat().st_size == expected_size and sha(archive) == row["archive_sha256"],
            "downloaded archive differs")
    tar = destination / "ordinary.tar.zst"
    with ZipFile(archive) as files:
        member, = files.infolist()
        require(member.filename == "integration-cli.tar.zst" and 0 < member.file_size <= MAX_ARCHIVE,
                "integration CLI ZIP member differs")
        with files.open(member) as source, tar.open("xb") as output:
            shutil.copyfileobj(source, output, length=1024**2)
    require(sha(tar) == row["zst_sha256"], "ordinary inner archive differs")
    binary = destination / "rift"
    with subprocess.Popen(["zstd", "-dc", str(tar)], stdout=subprocess.PIPE) as child:
        try:
            with tarfile.open(fileobj=child.stdout, mode="r|") as members:
                item = members.next()
                require(item is not None and item.name == "rift" and item.isfile()
                        and 0 < item.size == row["binary_size"] <= MAX_BINARY and item.mode & 0o111,
                        "unexpected CLI tar member")
                with members.extractfile(item) as source, binary.open("xb") as output:
                    shutil.copyfileobj(source, output, length=1024**2)
                require(members.next() is None, "additional tar members are outside artifact scope")
            require(child.wait(timeout=15) == 0, "CLI decompression failed")
        finally:
            if child.poll() is None:
                child.kill()
                child.wait(timeout=5)
    binary.chmod(0o500)
    return sha(binary)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("contract", type=Path)
    parser.add_argument("output", type=Path, nargs="?")
    parser.add_argument("--check-plan", action="store_true")
    args = parser.parse_args()
    contract = json.loads(args.contract.read_text())
    check_contract(contract, not args.check_plan)
    if args.check_plan:
        print("artifact-only plan schema accepted; candidate artifact still needs a reviewed lock")
        return
    pairs = {label: contract[label] for label in ("base", "candidate")}
    inputs = {label: source_build_inputs(row) for label, row in pairs.items()}
    require(inputs["base"] == inputs["candidate"], "same-profile build source settings differ")
    require(shutil.disk_usage(args.output.parent).free >= 11 * 1024**3, "artifact and Bun disk reserve absent")
    args.output.mkdir()
    qualify_source_workflow(contract, args.output)
    result = {"profile": "corpus", "build_input_blobs": inputs["base"], "arms": {}}
    actual = {}
    for label, row in pairs.items():
        destination = args.output / label
        destination.mkdir()
        artifact = verify_run(row, destination)
        download(row, destination / "ordinary.zip")
        binary_digest = extract(row, destination, artifact["size_in_bytes"])
        require(binary_digest == row["binary_sha256"], "installed executable bytes differ from the reviewed ordinary artifact")
        actual[label] = actual_profile(row, destination)
        result["arms"][label] = row
    require(actual["base"].keys() == actual["candidate"].keys() and all(
        actual["base"][key] == actual["candidate"][key] for key in actual["base"] if key != "image"),
        "observed toolchain, build settings or executable runtime ABI differ")
    result["ordinary_build_image"] = {label: row["image"] for label, row in actual.items()}
    (args.output / "verified-pair.json").write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()
