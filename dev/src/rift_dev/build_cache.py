"""Start sccache against the R2 build cache for the rest of a CI job.

The calling step maps the R2 secrets into this process's environment under the
secrets' own names, and no other step receives them. The sccache server started
here inherits the key and never idles out, so every later `rustc` call in the
job is a client that holds none. The secret scope decides the key: the Actions
scope holds the write pair, so a push or a pull request from this repository
stores what it compiles, and the Dependabot scope holds the read pair, so a
dependency bump reads the cache and stores nothing. A job that receives
neither - a pull request from a fork - compiles without sccache and says so.
"""

from __future__ import annotations

import os
from collections.abc import Mapping
from pathlib import Path
from typing import Literal

from pydantic import BaseModel, ConfigDict

from rift_dev.commands import Command, CommandFailed

BUCKET = "rift-oss-build-cache"
KEY_PREFIX = "sccache"
ENDPOINT = "R2_BUILD_CACHE_ENDPOINT"
SECRET_PREFIX = "R2_BUILD_CACHE_"

Mode = Literal["READ_WRITE", "READ_ONLY"]

# Each sccache mode, and the secrets holding the R2 key it runs with.
PAIRS: dict[Mode, tuple[str, str]] = {
    "READ_WRITE": (
        "R2_BUILD_CACHE_WRITE_ACCESS_KEY_ID",
        "R2_BUILD_CACHE_WRITE_SECRET_ACCESS_KEY",
    ),
    "READ_ONLY": (
        "R2_BUILD_CACHE_READ_ACCESS_KEY_ID",
        "R2_BUILD_CACHE_READ_SECRET_ACCESS_KEY",
    ),
}


class Credentials(BaseModel):
    """The one R2 key a job received, and the mode it grants."""

    model_config = ConfigDict(extra="forbid", frozen=True)
    endpoint: str
    access_key_id: str
    secret_access_key: str
    mode: Mode


def credentials(environment: Mapping[str, str]) -> Credentials | None:
    """The R2 key the step received, or `None` when it received no key.

    A job receives one pair, so both pairs, half a pair, or a key without the
    endpoint is a workflow defect and fails the step.
    """
    supplied = {
        mode: (environment.get(key_id, ""), environment.get(secret, ""))
        for mode, (key_id, secret) in PAIRS.items()
        if environment.get(key_id) or environment.get(secret)
    }
    if not supplied:
        return None
    if len(supplied) > 1:
        raise ValueError("a job receives the read pair or the write pair, never both")
    [(mode, (access_key_id, secret_access_key))] = supplied.items()
    if not access_key_id or not secret_access_key:
        raise ValueError(f"the {mode} key arrived without its other half")
    endpoint = environment.get(ENDPOINT, "")
    if not endpoint:
        raise ValueError(f"{ENDPOINT} is required beside an R2 key")
    return Credentials(
        endpoint=endpoint,
        access_key_id=access_key_id,
        secret_access_key=secret_access_key,
        mode=mode,
    )


def server_environment(
    environment: Mapping[str, str], selected: Credentials
) -> dict[str, str]:
    """The sccache server's environment: the job's own, with the R2 secrets
    replaced by the configuration sccache reads."""
    inherited = {
        name: value
        for name, value in environment.items()
        if not name.startswith(SECRET_PREFIX)
    }
    return inherited | {
        "SCCACHE_BUCKET": BUCKET,
        "SCCACHE_S3_KEY_PREFIX": KEY_PREFIX,
        "SCCACHE_REGION": "auto",
        "SCCACHE_ENDPOINT": selected.endpoint,
        "SCCACHE_S3_RW_MODE": selected.mode,
        "SCCACHE_IDLE_TIMEOUT": "0",
        "AWS_ACCESS_KEY_ID": selected.access_key_id,
        "AWS_SECRET_ACCESS_KEY": selected.secret_access_key,
    }


def main() -> int:
    """Start the server and route the job's later compiles through it."""
    environment = os.environ
    exported = environment.get("GITHUB_ENV")
    if not exported:
        raise ValueError(
            "GITHUB_ENV names the file a CI step exports variables through"
        )
    selected = credentials(environment)
    if selected is None:
        print("::notice::R2 build cache secrets are absent; compiling without sccache")
        return 0
    try:
        Command("sccache", "--start-server").with_environment(
            server_environment(environment, selected)
        ).run()
    except CommandFailed:
        print(
            "::warning::sccache did not start against the R2 build cache; compiling without it"
        )
        return 0
    # sccache caches no crate compiled incrementally.
    with Path(exported).open("a", encoding="utf-8") as file:
        file.write("RUSTC_WRAPPER=sccache\nCARGO_INCREMENTAL=0\n")
    print(f"sccache serves {selected.mode} from {BUCKET}/{KEY_PREFIX}", flush=True)
    return 0
