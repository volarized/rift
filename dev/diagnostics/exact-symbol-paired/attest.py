"""Authenticate the two pre-existing public CI artifact binaries before execution."""

from __future__ import annotations

import hashlib
import json
import shutil
import stat
import struct
import subprocess
import tarfile
from pathlib import Path
from zipfile import ZipFile

ARCHIVE_MEMBER = "integration-cli.tar.zst"
BINARY_MEMBER = "rift"
BINARY_MAX = 512 << 20


def require(condition: bool, detail: str) -> None:
    if not condition:
        raise ValueError(detail)


def digest(path: Path) -> str:
    with path.open("rb") as archive:
        return hashlib.file_digest(archive, "sha256").hexdigest()


def api(repository: str, resource: str) -> dict:
    result = subprocess.check_output(
        ["gh", "api", f"repos/{repository}/{resource}"], timeout=30
    )
    return json.loads(result)


def exact_archive_metadata(lock: dict, side: str) -> dict:
    expected = lock[side]
    repository = lock["repository"]
    run = api(repository, f"actions/runs/{expected['run']}")
    job = api(repository, f"actions/jobs/{expected['job']}")
    artifact = api(repository, f"actions/artifacts/{expected['artifact']}")
    head = api(repository, f"commits/{expected['head']}")
    source = api(repository, f"commits/{expected.get('synthetic', expected['head'])}")
    require(
        run["id"] == expected["run"] == job["run_id"] == artifact["workflow_run"]["id"],
        "workflow, job and artifact must share the locked run",
    )
    require(
        run["head_sha"]
        == job["head_sha"]
        == artifact["workflow_run"]["head_sha"]
        == expected["head"],
        "the archive must carry the locked workflow head",
    )
    require(
        run["run_attempt"] == 1
        and run["event"] == expected["event"]
        and run["path"] == ".github/workflows/ci.yml",
        "the source must be attempt one of standard CI",
    )
    require(
        job["name"] == "integration-cli"
        and job["status"] == "completed"
        and job["conclusion"] == "success"
        and job["labels"] == [lock["runner"]],
        "the public archive producer must be the successful standard runner job",
    )
    require(
        any(
            step["name"] == "Build and archive the CLI"
            and step["conclusion"] == "success"
            for step in job["steps"]
        ),
        "the runner must have built the CLI rather than loading a cache",
    )
    require(
        head["sha"] == expected["head"]
        and head["commit"]["tree"]["sha"] == expected["tree"],
        "the requested signed public head must have the locked product tree",
    )
    require(
        head["commit"]["verification"]["verified"],
        "the public source head is not verified",
    )
    require(
        source["commit"]["tree"]["sha"] == expected["tree"],
        "the built source has a different tree",
    )
    if side == "candidate":
        require(
            source["sha"] == expected["synthetic"]
            and [parent["sha"] for parent in source["parents"]]
            == [lock["baseline"]["head"], expected["head"]],
            "the pull-request source must be the locked synthetic two-parent commit",
        )
    require(
        artifact["name"] == "integration-cli"
        and not artifact["expired"]
        and artifact["size_in_bytes"] == expected["bytes"]
        and artifact["digest"] == "sha256:" + expected["sha256"],
        "the artifact service does not report the locked archive length and hash",
    )
    return {
        "head": head["sha"],
        "tree": expected["tree"],
        "build_source": source["sha"],
        "run": run["id"],
        "job": job["id"],
        "artifact": artifact["id"],
        "zip_sha256": expected["sha256"],
    }


def download(lock: dict, side: str, output: Path) -> tuple[Path, dict]:
    proof = exact_archive_metadata(lock, side)
    archive = output / (side + ".zip")
    with archive.open("xb") as destination:
        subprocess.run(
            [
                "gh",
                "api",
                f"repos/{lock['repository']}/actions/artifacts/{proof['artifact']}/zip",
            ],
            stdout=destination,
            check=True,
            timeout=180,
        )
    return archive, proof


def extract(archive: Path, output: Path, expected: dict) -> tuple[Path, dict]:
    require(
        archive.stat().st_size == expected["bytes"]
        and digest(archive) == expected["sha256"],
        "uploaded ZIP failed the immutable length or hash check",
    )
    packed = output / ARCHIVE_MEMBER
    with ZipFile(archive) as zip_file:
        entries = zip_file.infolist()
        require(
            len(entries) == 1 and entries[0].filename == ARCHIVE_MEMBER,
            "unexpected archive members",
        )
        entry = entries[0]
        require(
            not entry.is_dir()
            and not stat.S_ISLNK(entry.external_attr >> 16)
            and 0 < entry.file_size <= 200 << 20,
            "invalid compressed CLI member",
        )
        with zip_file.open(entry) as source, packed.open("xb") as destination:
            shutil.copyfileobj(source, destination)
    binary = output / BINARY_MEMBER
    with subprocess.Popen(
        ["zstd", "-dc", str(packed)], stdout=subprocess.PIPE
    ) as decoder:
        require(decoder.stdout is not None, "the archive decoder has no stream")
        try:
            with tarfile.open(
                fileobj=decoder.stdout, mode="r|", ignore_zeros=False
            ) as tar:
                member = tar.next()
                require(
                    member is not None
                    and member.name == BINARY_MEMBER
                    and member.isreg()
                    and 0 < member.size <= BINARY_MAX,
                    "unexpected tar binary member",
                )
                with (
                    tar.extractfile(member) as source,
                    binary.open("xb") as destination,
                ):
                    shutil.copyfileobj(source, destination)
                require(tar.next() is None, "unexpected extra tar members")
            require(decoder.wait(timeout=30) == 0, "invalid compressed tar stream")
        except BaseException:
            if decoder.poll() is None:
                decoder.kill()
            raise
    with binary.open("rb") as source:
        header = source.read(64)
    require(
        header[:6] == b"\x7fELF\x02\x01"
        and len(header) == 64
        and struct.unpack_from("<H", header, 18)[0] == 62,
        "expected a native Linux x86_64 CLI",
    )
    binary.chmod(0o755)
    return binary, {
        "tar_sha256": digest(packed),
        "binary_sha256": digest(binary),
        "binary_bytes": binary.stat().st_size,
    }
