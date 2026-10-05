use rift_error::__rift_error_definition;

__rift_error_definition!(
    blob_too_large,
    slug = "rift.history.blob_too_large",
    message = "committed file {path} has {size} bytes, above accepted limit {bytes_max}",
    action = "use a revision with a smaller file",
    fields = {
        bytes_max: required(unsigned),
        path: required(path),
        size: required(unsigned),
    },
);

__rift_error_definition!(
    contribution_invalid,
    slug = "rift.history.contribution_invalid",
    message = "history Contribution conversion rejected: {detail}",
    action = "check the history Contribution data and retry",
    fields = {
        detail: required(string),
    },
);

__rift_error_definition!(
    path_unrepresentable,
    slug = "rift.history.path_unrepresentable",
    message = "committed path cannot be represented as UTF-8: {path}",
    action = "rename the committed path to valid UTF-8 and retry",
    fields = {
        path: required(string),
    },
);

__rift_error_definition!(
    revision_not_commit,
    slug = "rift.history.revision_not_commit",
    message = "revision {rev} resolves to {resolved_kind}, not a commit",
    action = "use a revision that names a commit",
    fields = {
        requires: required(string),
        resolved_kind: required(string),
        rev: required(string),
    },
);

__rift_error_definition!(
    revision_unknown,
    slug = "rift.history.revision_unknown",
    message = "revision does not resolve: {rev}",
    action = "use a branch, tag, or commit id this repository resolves",
    fields = {
        requires: required(string),
        rev: required(string),
    },
);

__rift_error_definition!(
    storage,
    slug = "rift.history.storage",
    message = "repository storage failed during {operation}: {detail}",
    action = "check repository storage and retry",
    fields = {
        detail: required(string),
        operation: required(string),
    },
);

__rift_error_definition!(
    too_many_tags,
    slug = "rift.history.too_many_tags",
    message = "repository tag count exceeds accepted limit {tags_max}",
    action = "reduce repository tags or raise the tag limit",
    fields = {
        limit: required(string),
        tags_max: required(unsigned),
    },
);

__rift_error_definition!(
    tree_too_large,
    slug = "rift.history.tree_too_large",
    message = "revision tree exceeds accepted entry limit {entries_max}",
    action = "use a revision with a smaller tree",
    fields = {
        entries_max: required(unsigned),
        limit: required(string),
    },
);

__rift_error_definition!(
    unversioned,
    slug = "rift.history.unversioned",
    message = "workspace has no git repository: {workspace}",
    action = "run `git init`, or omit `rev` to read current tree",
    fields = {
        requires: required(string),
        workspace: required(path),
    },
);
