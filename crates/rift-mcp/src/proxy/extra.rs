//! Named project roots accepted once for one proxy connection.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rift_error::{RiftError, errors};
use rift_protocol::configuration::McpConfiguration;
#[cfg(test)]
mod tests;

/// Canonical roots retained without reading extra project configurations.
#[derive(Debug, Default)]
pub(super) struct Registrations {
    roots: BTreeMap<String, PathBuf>,
    primary: Option<PathBuf>,
}

impl Registrations {
    pub(super) fn accept(
        root: &Path,
        saved: &McpConfiguration,
        cli: &[String],
        home: Option<&Path>,
    ) -> Result<Self, RiftError> {
        let extra = merged(saved, cli)?;
        let primary = root.canonicalize().map_err(|error| {
            invalid("", &format!("primary root could not be resolved: {error}"))
        })?;
        let mut roots = BTreeMap::new();
        let mut names = BTreeMap::new();
        for (name, path) in extra.extra {
            let path = resolve_path(root, &name, &path, home)?;
            if path == primary {
                return Err(invalid(&name, "registration names the primary root"));
            }
            if let Some(first) = names.insert(path.clone(), name.clone()) {
                return Err(errors::core::configuration_invalid()
                    .field("mcp.extra")
                    .name(name)
                    .first(first)
                    .detail("registrations name the same canonical root")
                    .error());
            }
            roots.insert(name, path);
        }
        Ok(Self {
            roots,
            primary: Some(primary),
        })
    }

    pub(super) fn primary_root(&self) -> Option<&Path> {
        self.primary.as_deref()
    }

    pub(super) fn root(&self, name: &str) -> Option<&Path> {
        self.roots.get(name).map(PathBuf::as_path)
    }

    pub(super) fn names(&self) -> impl Iterator<Item = &str> {
        self.roots.keys().map(String::as_str)
    }

    pub(super) fn entries(&self) -> impl Iterator<Item = (&str, &Path)> {
        self.roots
            .iter()
            .map(|(name, root)| (name.as_str(), root.as_path()))
    }

    pub(super) fn refusal(&self, name: &str, tool: &str, field: &str) -> RiftError {
        if self.root(name).is_none() {
            return errors::mcp::parameter_invalid()
                .tool(tool)
                .field(field)
                .accepted(
                    self.names()
                        .map(|name| format!("local@{name}"))
                        .collect::<Vec<_>>()
                        .join(", "),
                )
                .error();
        }
        errors::server::read_unavailable()
            .operation(tool)
            .detail(format!(
                "registered project {name} has no available backend"
            ))
            .error()
    }
}

fn merged(saved: &McpConfiguration, cli: &[String]) -> Result<McpConfiguration, RiftError> {
    let mut supplied = BTreeMap::new();
    for entry in cli {
        let Some((name, path)) = entry.split_once('=') else {
            return Err(invalid("", "registration must have name=path form"));
        };
        if supplied.insert(name.to_owned(), path.to_owned()).is_some() {
            return Err(invalid(name, "CLI registrations repeat this name"));
        }
    }
    let mut extra = saved.clone();
    extra.extra.extend(supplied);
    extra.validate()?;
    Ok(extra)
}

fn resolve_path(
    root: &Path,
    name: &str,
    path: &str,
    home: Option<&Path>,
) -> Result<PathBuf, RiftError> {
    let resolved = if path == "~" {
        home.ok_or_else(|| invalid(name, "home directory is unavailable"))?
            .to_path_buf()
    } else if let Some(relative) = path.strip_prefix("~/") {
        home.ok_or_else(|| invalid(name, "home directory is unavailable"))?
            .join(relative)
    } else {
        let path = Path::new(path);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            root.join(path)
        }
    };
    let canonical = resolved.canonicalize().map_err(|error| {
        invalid(
            name,
            &format!("registration path could not be resolved: {error}"),
        )
    })?;
    if !canonical.is_dir() {
        return Err(invalid(name, "registration path is not a directory"));
    }
    Ok(canonical)
}

fn invalid(name: &str, detail: &str) -> RiftError {
    errors::core::configuration_invalid()
        .field("mcp.extra")
        .name(name)
        .detail(detail)
        .error()
}
