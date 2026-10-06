use rift_error::__rift_error_definition;

__rift_error_definition!(
    install_home_unresolved,
    slug = "rift.cli.install_home_unresolved",
    message = "the operator's home directory could not be resolved",
    action = "set HOME (or USERPROFILE on Windows) and retry `rift install claude --user`",
    fields = {
        checked: required(string),
    },
);

__rift_error_definition!(
    install_remove_failed,
    slug = "rift.cli.install_remove_failed",
    message = "the generated Claude Code skill could not be removed: {path}",
    action = "ensure the target directory is writable and retry `rift install claude --remove`",
    fields = {
        path: required(path),
        source: required(source),
    },
);

__rift_error_definition!(
    install_settings_unparsable,
    slug = "rift.cli.install_settings_unparsable",
    message = "the target settings.json could not be read as a JSON hook document: {path}",
    action = "fix or remove the file, then run the same `rift install` command again",
    fields = {
        path: required(path),
        source: required(source),
    },
);

__rift_error_definition!(
    install_template_missing_tool,
    slug = "rift.cli.install_template_missing_tool",
    message = "the generated Claude Code skill names a tool the served MCP surface does not have: {tool}",
    action = "rebuild rift so the binary and its served tool surface match, then retry `rift install claude`",
    fields = {
        tool: required(string),
    },
);

__rift_error_definition!(
    install_write_failed,
    slug = "rift.cli.install_write_failed",
    message = "the generated Claude Code skill could not be written: {path}",
    action = "ensure the target directory is writable and retry `rift install claude`",
    fields = {
        path: required(path),
        source: required(source),
    },
);

__rift_error_definition!(
    server_already_serving,
    slug = "rift.cli.server_already_serving",
    message = "another rift server already serves this workspace",
    action = "connect to the listed server, or run `rift server stop` before serving again",
    fields = {
        detail: optional(string),
        listening: optional(string),
        pid: optional(pid),
    },
);

__rift_error_definition!(
    server_election_unreleased,
    slug = "rift.cli.server_election_unreleased",
    message = "server stopped answering its port but still holds the election: process {pid}, waited {waited}",
    action = "retry `rift server stop`; if the refusal repeats, end the reported pid manually",
    fields = {
        pid: required(pid),
        waited: required(duration),
    },
);

__rift_error_definition!(
    server_logs_unavailable,
    slug = "rift.cli.server_logs_unavailable",
    message = "the workspace's recorded server logs could not be read",
    action = "ensure no other process holds `.rift/metrics` exclusively and retry",
    fields = {
        detail: optional(string),
        operation: optional(string),
        source: optional(cause),
    },
);

__rift_error_definition!(
    server_spawn_failed,
    slug = "rift.cli.server_spawn_failed",
    message = "the rift server process could not be started: {source}",
    action = "check that the rift binary is runnable, or run `rift server start --foreground` to serve in this process",
    fields = {
        operation: required(string),
        source: required(source),
    },
);

__rift_error_definition!(
    server_start_exited,
    slug = "rift.cli.server_start_exited",
    message = "the spawned rift server exited before publishing its lock document: process {pid}",
    action = "read `.rift/server.stderr`, or run `rift server logs --level error`, for what the server reported before it exited",
    fields = {
        pid: required(pid),
    },
);

__rift_error_definition!(
    server_start_timed_out,
    slug = "rift.cli.server_start_timed_out",
    message = "the started rift server did not report serving within {waited}",
    action = "run `rift server start --foreground` to see the server's diagnostics on stderr",
    fields = {
        waited: required(duration),
    },
);

__rift_error_definition!(
    server_stop_refused,
    slug = "rift.cli.server_stop_refused",
    message = "the server refused the stop request with status {status}",
    action = "retry `rift server stop`; if the refusal repeats, end the reported pid manually",
    fields = {
        detail: optional(string),
        status: required(unsigned),
    },
);

__rift_error_definition!(
    server_stop_request_failed,
    slug = "rift.cli.server_stop_request_failed",
    message = "the server stop request could not be delivered: {source}",
    action = "retry `rift server stop`; if the refusal repeats, end the reported pid manually",
    fields = {
        operation: required(string),
        source: required(source),
    },
);

__rift_error_definition!(
    server_stop_timed_out,
    slug = "rift.cli.server_stop_timed_out",
    message = "the server accepted the stop request but kept serving after {waited}",
    action = "retry `rift server stop`; if the refusal repeats, end the reported pid manually",
    fields = {
        detail: optional(string),
        listening: optional(string),
        pid: optional(pid),
        waited: required(duration),
    },
);

__rift_error_definition!(
    update_archive_contents_invalid,
    slug = "rift.cli.update_archive_contents_invalid",
    message = "release archive contents are invalid: expected exactly one binary, README.md, and LICENSE.md member; retry `rift update`",
    action = "retry `rift update`",
    fields = {},
);

__rift_error_definition!(
    update_archive_extraction_failed,
    slug = "rift.cli.update_archive_extraction_failed",
    message = "release archive could not be extracted: retry `rift update`; if this persists the download may be corrupted",
    action = "retry `rift update`; if this persists the download may be corrupted",
    fields = {
        source: required(source),
    },
);

__rift_error_definition!(
    update_archive_file_inspection_failed,
    slug = "rift.cli.update_archive_file_inspection_failed",
    message = "downloaded release file at `{path}` could not be inspected: {source}: retry `rift update`",
    action = "retry `rift update`",
    fields = {
        path: required(path),
        source: required(source),
    },
);

__rift_error_definition!(
    update_archive_file_not_regular,
    slug = "rift.cli.update_archive_file_not_regular",
    message = "downloaded release file at `{path}` is not a regular file: retry `rift update` or create an issue at https://github.com/volarized/rift/issues",
    action = "retry `rift update` or create an issue",
    fields = {
        path: required(path),
    },
);

__rift_error_definition!(
    update_archive_file_size_invalid,
    slug = "rift.cli.update_archive_file_size_invalid",
    message = "downloaded release file at `{path}` has incorrect size of {size} bytes, expected between 1 and {bytes_max} bytes: retry `rift update` or create an issue at https://github.com/volarized/rift/issues",
    action = "retry `rift update` or create an issue",
    fields = {
        bytes_max: required(unsigned),
        path: required(path),
        size: required(unsigned),
    },
);

__rift_error_definition!(
    update_archive_member_too_large,
    slug = "rift.cli.update_archive_member_too_large",
    message = "release archive member is empty or exceeds {bytes_max} bytes: retry `rift update`; if this persists the release may be malformed",
    action = "retry `rift update`; if this persists the release may be malformed",
    fields = {
        bytes_max: required(unsigned),
    },
);

__rift_error_definition!(
    update_binary_invalid,
    slug = "rift.cli.update_binary_invalid",
    message = "current Rift executable (invoked as `{invoked_as}`) could not be located: {source}: reinstall Rift if the binary was moved or deleted",
    action = "reinstall Rift if the binary was moved or deleted",
    fields = {
        invoked_as: required(string),
        source: required(source),
    },
);

__rift_error_definition!(
    update_checksum_manifest_invalid,
    slug = "rift.cli.update_checksum_manifest_invalid",
    message = "release checksum manifest is invalid: expected `sha256sum`-format lines naming the release archive exactly once; retry `rift update`",
    action = "retry `rift update`",
    fields = {},
);

__rift_error_definition!(
    update_checksum_mismatch,
    slug = "rift.cli.update_checksum_mismatch",
    message = "the downloaded release does not match its published checksum: expected {expected}, actual {actual}; retry `rift update`, and raise an issue at https://github.com/volarized/rift/issues if the mismatch repeats",
    action = "retry `rift update` and raise an issue if mismatch repeats",
    fields = {
        actual: required(string),
        expected: required(string),
    },
);

__rift_error_definition!(
    update_checksum_read_failed,
    slug = "rift.cli.update_checksum_read_failed",
    message = "release checksum could not be verified: retry `rift update`",
    action = "retry `rift update`",
    fields = {
        source: required(source),
    },
);

__rift_error_definition!(
    update_download_failed,
    slug = "rift.cli.update_download_failed",
    message = "release download failed: check network access to github.com and retry `rift update`",
    action = "check network access to github.com and retry `rift update`",
    fields = {
        source: required(source),
    },
);

__rift_error_definition!(
    update_download_too_large,
    slug = "rift.cli.update_download_too_large",
    message = "release download was empty or exceeded {bytes_max} bytes: retry `rift update`; if this persists the release assets may be malformed",
    action = "retry `rift update`; if this persists the release assets may be malformed",
    fields = {
        bytes_max: required(unsigned),
    },
);

__rift_error_definition!(
    update_prerelease_unsupported,
    slug = "rift.cli.update_prerelease_unsupported",
    message = "release tag `{tag}` is a pre-release or build: only stable releases of the form `vMAJOR.MINOR.PATCH` are supported",
    action = "use a stable release tag of the form `vMAJOR.MINOR.PATCH`",
    fields = {
        tag: required(string),
    },
);

__rift_error_definition!(
    update_publish_copy_failed,
    slug = "rift.cli.update_publish_copy_failed",
    message = "Rift update could not be published: copying the downloaded binary into `{path}` failed: ensure the directory is writable and has free space, then retry `rift update`",
    action = "ensure the directory is writable and has free space, then retry `rift update`",
    fields = {
        cause: required(cause),
        path: required(path),
    },
);

__rift_error_definition!(
    update_publish_failed,
    slug = "rift.cli.update_publish_failed",
    message = "Rift update could not be published: {operation} `{path}` failed: {source}: ensure the directory is writable and retry `rift update`",
    action = "ensure the directory is writable and retry `rift update`",
    fields = {
        operation: required(string),
        path: required(path),
        source: required(source),
    },
);

__rift_error_definition!(
    update_publish_parent_missing,
    slug = "rift.cli.update_publish_parent_missing",
    message = "current executable `{path}` has no parent directory: install Rift in a regular directory before updating",
    action = "install Rift in a regular directory before updating",
    fields = {
        path: required(path),
    },
);

__rift_error_definition!(
    update_publish_pending_cleanup,
    slug = "rift.cli.update_publish_pending_cleanup",
    message = "another Rift update is pending cleanup: retry after the previous Rift process exits, or delete `{path}`",
    action = "retry after the previous Rift process exits, or delete the pending file",
    fields = {
        path: required(path),
    },
);

__rift_error_definition!(
    update_release_file_inspection_failed,
    slug = "rift.cli.update_release_file_inspection_failed",
    message = "downloaded release file at `{path}` could not be inspected: {source}: retry `rift update`",
    action = "retry `rift update`",
    fields = {
        path: required(path),
        source: required(source),
    },
);

__rift_error_definition!(
    update_release_file_not_regular,
    slug = "rift.cli.update_release_file_not_regular",
    message = "downloaded release file at `{path}` is not a regular file: retry `rift update` or create an issue at https://github.com/volarized/rift/issues",
    action = "retry `rift update` or create an issue",
    fields = {
        path: required(path),
    },
);

__rift_error_definition!(
    update_release_file_size_invalid,
    slug = "rift.cli.update_release_file_size_invalid",
    message = "downloaded release file at `{path}` has incorrect size of {size} bytes, expected between 1 and {bytes_max} bytes: retry `rift update` or create an issue at https://github.com/volarized/rift/issues",
    action = "retry `rift update` or create an issue",
    fields = {
        bytes_max: required(unsigned),
        path: required(path),
        size: required(unsigned),
    },
);

__rift_error_definition!(
    update_release_metadata_invalid,
    slug = "rift.cli.update_release_metadata_invalid",
    message = "latest release metadata is invalid: retry `rift update` or check https://github.com/volarized/rift/releases",
    action = "retry `rift update` or check the release page",
    fields = {
        source: required(source),
    },
);

__rift_error_definition!(
    update_release_tag_invalid,
    slug = "rift.cli.update_release_tag_invalid",
    message = "release tag `{tag}` is invalid: expected the form `vMAJOR.MINOR.PATCH`, such as `v0.0.2`",
    action = "use a stable release tag of the form `vMAJOR.MINOR.PATCH`",
    fields = {
        source: optional(source),
        tag: required(string),
    },
);

__rift_error_definition!(
    update_rollback_cleanup_failed,
    slug = "rift.cli.update_rollback_cleanup_failed",
    message = "We were not able to clean up the old binary at `{path}`: {source}: delete the file manually",
    action = "delete the file manually",
    fields = {
        path: required(path),
        source: required(source),
    },
);

__rift_error_definition!(
    update_rollback_failed,
    slug = "rift.cli.update_rollback_failed",
    message = "Rift update publish and rollback of `{path}` both failed: reinstall Rift from an official release",
    action = "reinstall Rift from an official release",
    fields = {
        path: required(path),
        source: required(source),
    },
);

__rift_error_definition!(
    update_staging_failed,
    slug = "rift.cli.update_staging_failed",
    message = "update staging directory could not be created under `{path}` ({space}): {source}: ensure the directory is writable and has free space, then retry `rift update`",
    action = "ensure the directory is writable and has free space, then retry `rift update`",
    fields = {
        path: required(path),
        source: required(source),
        space: required(string),
    },
);

__rift_error_definition!(
    update_version_invalid,
    slug = "rift.cli.update_version_invalid",
    message = "installed Rift version `{raw}` at `{path}` is invalid: {source}: reinstall Rift from an official release",
    action = "reinstall Rift from an official release",
    fields = {
        path: required(path),
        raw: required(string),
        source: required(source),
    },
);
