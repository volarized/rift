//! Named project roots accepted once for one proxy connection.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rift_error::{ErrorContext, RiftError, errors};
use rift_protocol::configuration::McpConfiguration;
use rift_protocol::identity::{PROFILE_SEPARATOR, SymbolOwner};
#[cfg(test)]
mod tests;

/// Canonical roots retained for one connection, keyed by backend name.
///
/// An extra project's own profiles are registered beside it; their own extra
/// registrations are not followed. An extra project whose configuration is refused
/// stays registered without profiles, and the refusal answers its profile scopes.
#[derive(Debug, Default)]
pub(super) struct Registrations {
    roots: BTreeMap<String, PathBuf>,
    primary: Option<PathBuf>,
    refused: BTreeMap<String, RiftError>,
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
            let saved_path = saved.extra.get(&name);
            let resolved = resolve_path(root, &name, &path, home)?;
            if resolved == primary {
                return Err(invalid(&name, "registration names the primary root"));
            }
            if saved_path != Some(&path) {
                let replaced =
                    saved_path.and_then(|saved| resolve_path(root, &name, saved, home).ok());
                if let Some(detail) = served_twice(&primary, &resolved, replaced.as_deref()) {
                    return Err(invalid(&name, detail));
                }
            }
            let path = resolved;
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
            refused: BTreeMap::new(),
        })
    }

    /// Registers the primary project's profiles and each extra project's own profiles.
    ///
    /// An extra project's profiles come from the configuration its own server accepts at
    /// its root, so each profile is served with that profile's root and settings. An extra
    /// project whose configuration is refused keeps its registration and serves no profile.
    ///
    /// # Errors
    /// Returns a configuration refusal for an invalid primary profile root, a profile that
    /// names an already registered root or the primary root, a primary profile overlapping
    /// an extra project or its profile, or an extra profile scope past the identity bound.
    pub(super) fn with_profiles(
        mut self,
        root: &Path,
        configuration: &rift_protocol::configuration::WorkspaceConfiguration,
    ) -> Result<Self, RiftError> {
        let primary_profiles = rift_server::profile_roots(root, configuration)?;
        let extra_profiles = self.extra_profiles()?;
        let extra_paths: Vec<&PathBuf> = self
            .roots
            .values()
            .chain(extra_profiles.iter().map(|(_, _, path)| path))
            .collect();
        if let Some(name) = overlapping_profile(&primary_profiles, &extra_paths) {
            return errors::core::configuration_invalid()
                .field("profiles")
                .name(name)
                .detail("profile overlaps an extra project; select separate project folders")
                .fail();
        }
        let profiles = primary_profiles
            .into_iter()
            .map(|(name, path)| (super::identity::profile_key(&name), name, path))
            .chain(extra_profiles);
        for (key, name, path) in profiles {
            self.register_profile(key, name, path)?;
        }
        Ok(self)
    }

    /// Each extra project's own profiles as backend key, profile name, and root. An extra
    /// project whose configuration is refused contributes none and keeps its refusal.
    fn extra_profiles(&mut self) -> Result<Vec<(String, String, PathBuf)>, RiftError> {
        let mut profiles = Vec::new();
        for (project, project_root) in &self.roots {
            let roots = match extra_profile_roots(project_root) {
                Ok(roots) => roots,
                Err(error) => {
                    self.refused.insert(project.clone(), error);
                    continue;
                }
            };
            for (name, path) in roots {
                profiles.push((checked_profile_key(project, &name)?, name, path));
            }
        }
        Ok(profiles)
    }

    fn register_profile(
        &mut self,
        key: String,
        name: String,
        path: PathBuf,
    ) -> Result<(), RiftError> {
        let detail = if self.primary.as_ref() == Some(&path) {
            "profile names the primary project root"
        } else if self.roots.values().any(|other| other == &path) {
            "profile and extra workspace name the same canonical root"
        } else if let std::collections::btree_map::Entry::Vacant(entry) = self.roots.entry(key) {
            entry.insert(path);
            return Ok(());
        } else {
            "profile scope names an already registered project"
        };
        errors::core::configuration_invalid()
            .field("profiles")
            .name(name)
            .detail(detail)
            .fail()
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

    /// The refusal for a scope that selects no available backend.
    ///
    /// A profile of an extra project whose configuration was refused answers with that
    /// configuration refusal; another unknown scope lists the accepted ones.
    pub(super) fn refusal(&self, name: &str, tool: &str, field: &str) -> RiftError {
        let refused = name
            .split_once(PROFILE_SEPARATOR)
            .and_then(|(project, _)| self.refused.get(project));
        match (self.root(name), refused) {
            (Some(_), _) => errors::server::read_unavailable()
                .operation(tool)
                .detail(format!(
                    "registered project {name} has no available backend"
                ))
                .error(),
            (None, Some(error)) => error
                .clone()
                .with(ErrorContext::new("scope", format!("local@{name}"))),
            (None, None) => errors::mcp::parameter_invalid()
                .tool(tool)
                .field(field)
                .accepted(self.accepted_scopes())
                .error(),
        }
    }

    fn accepted_scopes(&self) -> String {
        self.names()
            .map(|name| {
                if name.starts_with(PROFILE_SEPARATOR) {
                    format!("local{name}")
                } else {
                    format!("local@{name}")
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The first primary profile whose root contains, or lies inside, another registered root.
fn overlapping_profile(primary: &BTreeMap<String, PathBuf>, others: &[&PathBuf]) -> Option<String> {
    primary.iter().find_map(|(name, profile)| {
        let overlaps = others.iter().any(|other| {
            *other != profile && (other.starts_with(profile) || profile.starts_with(other))
        });
        overlaps.then(|| name.clone())
    })
}

/// The profile roots an extra project's own server serves, or that project's configuration refusal.
fn extra_profile_roots(project_root: &Path) -> Result<BTreeMap<String, PathBuf>, RiftError> {
    let configuration = rift_server::load_configuration(project_root)?;
    rift_server::profile_roots(project_root, &configuration)
}

/// The backend key of one extra project's profile, refused when its `local@project:name`
/// scope exceeds the symbol identity byte bound.
fn checked_profile_key(project: &str, name: &str) -> Result<String, RiftError> {
    let owner = SymbolOwner::NamedProfileLocal {
        project: project.to_owned(),
        name: name.to_owned(),
    };
    match owner.local_scope() {
        Ok(_) => Ok(super::identity::extra_profile_key(project, name)),
        Err(_) => errors::core::configuration_invalid()
            .field("profiles")
            .name(super::identity::extra_profile_key(project, name))
            .detail("extra project profile scope exceeds the symbol identity byte bound")
            .fail(),
    }
}

/// Why a CLI registration would leave source the primary project also serves, if it would.
///
/// The primary project leaves only saved `[mcp.extra]` folders to their registrations, so a
/// CLI entry below the primary root, or one replacing a saved entry below it, is refused.
fn served_twice(primary: &Path, resolved: &Path, replaced: Option<&Path>) -> Option<&'static str> {
    if resolved.starts_with(primary) {
        return Some(
            "a CLI registration below the primary root is also served by the primary project; save it in mcp.extra",
        );
    }
    replaced.filter(|saved| saved.starts_with(primary)).map(|_| {
        "a CLI registration replaces a saved project below the primary root; save the change in mcp.extra"
    })
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
