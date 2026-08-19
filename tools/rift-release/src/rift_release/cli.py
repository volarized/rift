"""Command-line interface for Rift release tooling."""

from __future__ import annotations

from collections.abc import Callable
from pathlib import Path
from typing import Annotated, TypeVar

import typer

from . import release

app = typer.Typer(
    help="Validate and package Rift binary releases.",
    no_args_is_help=True,
)
DEFAULT_REPOSITORY = Path.cwd()

Result = TypeVar("Result")
Tag = Annotated[
    str,
    typer.Option(envvar="RIFT_TAG", help="Canonical vX.Y.Z release tag."),
]
Target = Annotated[
    str,
    typer.Option(envvar="RIFT_TARGET", help="Supported Rust target triple."),
]
Repository = Annotated[
    Path,
    typer.Option(file_okay=False, resolve_path=True, help="Repository root."),
]


def invoke(operation: Callable[[], Result]) -> Result:
    """Run one operation with concise command-line errors."""
    try:
        return operation()
    except (OSError, RuntimeError, ValueError) as error:
        typer.echo(f"error: {error}", err=True)
        raise typer.Exit(code=1) from error


@app.command("validate-tag")
def validate_tag(
    tag: Tag,
    repository: Repository = DEFAULT_REPOSITORY,
) -> None:
    """Validate tag against workspace package versions."""
    invoke(lambda: release.validate_workspace_version(repository, tag))


@app.command("verify-binary")
def verify_binary(
    binary: Annotated[Path, typer.Option(exists=True, dir_okay=False)],
    tag: Tag,
) -> None:
    """Verify binary version and help output."""
    invoke(lambda: release.verify_binary_version(binary, tag))


@app.command("package")
def package(
    binary: Annotated[Path, typer.Option(exists=True, dir_okay=False)],
    output: Annotated[Path, typer.Option(file_okay=False)],
    tag: Tag,
    target: Target,
    repository: Repository = DEFAULT_REPOSITORY,
) -> None:
    """Create deterministic archive for one target."""

    def create_archive() -> Path:
        release.validate_workspace_version(repository, tag)
        return release.package_release(repository, tag, target, binary, output)

    typer.echo(invoke(create_archive))


@app.command("checksums")
def checksums(
    directory: Annotated[Path, typer.Option(exists=True, file_okay=False)],
    tag: Tag,
) -> None:
    """Write checksums for complete target archive set."""
    typer.echo(invoke(lambda: release.checksum_manifest(tag, directory)))
