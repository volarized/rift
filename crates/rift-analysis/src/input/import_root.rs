use std::collections::BTreeSet;

use rift_core::ProjectPath;
use rift_error::{RiftError, errors};
use rift_protocol::{identity::SymbolOwner, index::python_identifier_is_valid, read::Language};

use super::PackageSource;

/// Maximum retained import roots and module entries for one selected artifact.
pub const PACKAGE_IMPORT_ENTRIES_MAX: usize = 64;
/// Maximum aggregate UTF-8 bytes in retained import roots and module names.
pub const PACKAGE_IMPORT_BYTES_MAX: usize = 65_536;

/// Runtime names accepted for a selected Python standard library.
const PYTHON_RUNTIME_NAMES: [&str; 2] = ["python", "cpython"];

/// Captured metadata that establishes an installed Python import root.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PackageImportRootOrigin {
    /// A wheel's validated installed layout.
    Wheel,
    /// A statically captured `py_modules` list.
    PyModules,
    /// A statically captured Flit module selection.
    Flit,
    /// A statically captured PDM package layout and build-path observations.
    Pdm,
    /// A selected Python standard library with an exact runtime owner.
    Runtime,
}

/// Original source prefix and established Python modules under that prefix.
///
/// Source paths remain unchanged. `None` names the captured artifact root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageImportRoot {
    prefix: Option<ProjectPath>,
    modules: Vec<String>,
    origin: PackageImportRootOrigin,
    runtime: Option<SymbolOwner>,
}

impl PackageImportRoot {
    /// Validates a bounded, unique module set and its original source prefix.
    ///
    /// # Errors
    ///
    /// Returns an identity error for empty or invalid modules, duplicate modules,
    /// an empty explicit prefix, or an exceeded count or byte bound.
    pub fn new(
        prefix: Option<ProjectPath>,
        modules: Vec<String>,
        origin: PackageImportRootOrigin,
    ) -> Result<Self, RiftError> {
        if origin == PackageImportRootOrigin::Runtime
            || modules.is_empty()
            || modules.len() > PACKAGE_IMPORT_ENTRIES_MAX
            || prefix.as_ref().is_some_and(|path| path.as_str().is_empty())
        {
            return invalid();
        }
        let mut bytes = prefix.as_ref().map_or(0, |path| path.as_str().len());
        for module in &modules {
            bytes = bytes
                .checked_add(module.len())
                .ok_or_else(|| errors::analysis::package_input_identity_invalid().error())?;
            if bytes > PACKAGE_IMPORT_BYTES_MAX
                || !module.split('.').all(python_identifier_is_valid)
            {
                return invalid();
            }
        }
        let unique = modules.iter().collect::<BTreeSet<_>>();
        if unique.len() != modules.len() {
            return invalid();
        }
        Ok(Self {
            prefix,
            modules,
            origin,
            runtime: None,
        })
    }

    /// Selects the original Python library layout under one exact runtime owner.
    ///
    /// The caller supplies the prefix admitted from the verified runtime archive.
    /// `None` names the selected library root after acquisition removed its archive prefix.
    /// Input validation requires the same owner and an admitted Python source under this root.
    /// `python` and `cpython` retain their supplied runtime names.
    ///
    /// # Errors
    /// Returns the existing identity refusal for a different runtime, invalid version,
    /// empty explicit prefix, or exceeded byte bound.
    pub fn runtime(owner: SymbolOwner, prefix: Option<ProjectPath>) -> Result<Self, RiftError> {
        owner.validate().map_err(|source| {
            errors::analysis::package_input_identity_invalid()
                .cause(source)
                .error()
        })?;
        if !matches!(&owner, SymbolOwner::Runtime { runtime, .. } if PYTHON_RUNTIME_NAMES.contains(&runtime.as_str()))
            || prefix.as_ref().is_some_and(|path| path.as_str().is_empty())
        {
            return invalid();
        }
        let root = Self {
            prefix,
            modules: Vec::new(),
            origin: PackageImportRootOrigin::Runtime,
            runtime: Some(owner),
        };
        if root.bytes() > PACKAGE_IMPORT_BYTES_MAX {
            return invalid();
        }
        Ok(root)
    }

    /// Original artifact-relative prefix, or the artifact root.
    #[must_use]
    pub const fn prefix(&self) -> Option<&ProjectPath> {
        self.prefix.as_ref()
    }

    /// Established Python module names.
    #[must_use]
    pub fn modules(&self) -> &[String] {
        &self.modules
    }

    /// Captured metadata that established this root.
    #[must_use]
    pub const fn origin(&self) -> PackageImportRootOrigin {
        self.origin
    }

    pub(super) fn bytes(&self) -> usize {
        self.prefix.as_ref().map_or(0, |path| path.as_str().len())
            + self.modules.iter().map(String::len).sum::<usize>()
            + self.runtime.as_ref().map_or(0, |owner| match owner {
                SymbolOwner::Runtime { runtime } => runtime.len(),
                _ => 0,
            })
    }
}

pub(crate) fn validate_roots(roots: &[PackageImportRoot]) -> Result<(), RiftError> {
    if roots.len() > PACKAGE_IMPORT_ENTRIES_MAX {
        return invalid();
    }
    let mut modules = 0_usize;
    let mut bytes = 0_usize;
    for root in roots {
        modules = modules
            .checked_add(root.modules.len())
            .ok_or_else(|| errors::analysis::package_input_identity_invalid().error())?;
        bytes = bytes
            .checked_add(root.bytes())
            .ok_or_else(|| errors::analysis::package_input_identity_invalid().error())?;
        if modules > PACKAGE_IMPORT_ENTRIES_MAX || bytes > PACKAGE_IMPORT_BYTES_MAX {
            return invalid();
        }
    }
    let mut prefixes = BTreeSet::new();
    for root in roots {
        if !prefixes.insert(root.prefix.as_ref()) {
            return invalid();
        }
    }
    Ok(())
}

fn invalid<T>() -> Result<T, RiftError> {
    errors::analysis::package_input_identity_invalid().fail()
}

#[cfg(test)]
mod tests;

/// Checks runtime layout authority against the exact selected input.
pub(crate) fn validate_runtime_roots(
    owner: &SymbolOwner,
    language: &Language,
    files: &[PackageSource<'_>],
    roots: &[PackageImportRoot],
) -> Result<(), RiftError> {
    for root in roots {
        if root.origin != PackageImportRootOrigin::Runtime {
            continue;
        }
        if root.runtime.as_ref() != Some(owner)
            || language.name != "python"
            || language.dialect.is_some()
            || !files.iter().any(|source| {
                let path = source.path().as_str();
                let relative = match root.prefix.as_ref() {
                    Some(prefix) => path
                        .strip_prefix(prefix.as_str())
                        .and_then(|tail| tail.strip_prefix('/')),
                    None => Some(path),
                };
                relative.is_some_and(|path| {
                    path.rsplit_once('.')
                        .is_some_and(|(_, extension)| matches!(extension, "py" | "pyi"))
                })
            })
        {
            return invalid();
        }
    }
    Ok(())
}
