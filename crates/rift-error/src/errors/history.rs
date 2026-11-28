error_namespace! {
    history;

    blob_too_large {
        message = "committed file {path} has {size} bytes, above accepted limit {bytes_max}",
        action = "use a revision with a smaller file",
        fields = {
            bytes_max: required(unsigned),
            path: required(path),
            size: required(unsigned),
        },
    }

    contribution_invalid {
        message = "history Contribution conversion rejected: {detail}",
        action = "check the history Contribution data and retry",
        fields = {
            detail: required(string),
        },
    }

    path_unrepresentable {
        message = "committed path cannot be represented as UTF-8: {path}",
        action = "rename the committed path to valid UTF-8 and retry",
        fields = {
            path: required(string),
        },
    }

    revision_not_commit {
        message = "revision {rev} resolves to {resolved_kind}, not a commit",
        action = "use a revision that names a commit",
        fields = {
            requires: required(string),
            resolved_kind: required(string),
            rev: required(string),
        },
    }

    revision_unknown {
        message = "revision does not resolve: {rev}",
        action = "use a branch, tag, or commit id this repository resolves",
        fields = {
            requires: required(string),
            rev: required(string),
        },
    }

    storage {
        message = "repository storage failed during {operation}: {detail}",
        action = "check repository storage and retry",
        fields = {
            detail: required(string),
            operation: required(string),
        },
    }

    too_many_tags {
        message = "repository tag count exceeds accepted limit {tags_max}",
        action = "reduce repository tags or raise the tag limit",
        fields = {
            limit: required(string),
            tags_max: required(unsigned),
        },
    }

    tree_too_large {
        message = "revision tree exceeds accepted entry limit {entries_max}",
        action = "use a revision with a smaller tree",
        fields = {
            entries_max: required(unsigned),
            limit: required(string),
        },
    }

    unversioned {
        message = "workspace has no git repository: {workspace}",
        action = "run `git init`, or omit `revision` to read current tree",
        fields = {
            requires: required(string),
            workspace: required(path),
        },
    }
}
