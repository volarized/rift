//! Logical stub package roots backed by captured wheel ownership.

use std::collections::BTreeSet;

use rift_dependency::{
    normalized_python_distribution_name, wheel_dist_info_name, wheel_record_paths,
};
use rift_protocol::{identity::SymbolOwner, index::python_identifier_is_valid};

use crate::{NamespaceInput, PackageImportRoot, PackageImportRootOrigin};

const STUB_SUFFIX: &str = "-stubs";

/// Reads stub roots from the selected wheel's captured release metadata.
pub(in crate::analyzer::namespace) fn stub_wheel_root(
    input: &NamespaceInput<'_>,
) -> Option<PackageImportRoot> {
    let SymbolOwner::Package { manager, name, .. } = input.owner() else {
        return None;
    };
    if manager != "pypi" || input.language().name != "python" {
        return None;
    }
    let version = input.release_version()?;
    let distribution = normalized_python_distribution_name(name);
    let directory = wheel_dist_info_name(&distribution, version);
    let metadata = captured_metadata(input, &directory, "METADATA")?;
    let metadata_name = field(metadata, "Name")?;
    if normalized_python_distribution_name(metadata_name) != distribution
        || field(metadata, "Version")? != version
    {
        return None;
    }
    let record = captured_metadata(input, &directory, "RECORD")?;
    let top_level = captured_metadata(input, &directory, "top_level.txt")?;
    let recorded = wheel_record_paths(record).collect::<BTreeSet<_>>();
    let mut roots = BTreeSet::new();
    for physical in top_level
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        let logical = physical.strip_suffix(STUB_SUFFIX)?;
        if !python_identifier_is_valid(logical)
            || !roots.insert(logical)
            || roots.len() > crate::PACKAGE_IMPORT_ENTRIES_MAX
        {
            return None;
        }
    }
    let mut witnessed = BTreeSet::new();
    for captured in input.files() {
        if super::super::python_script_origin(input, *captured).is_some() {
            continue;
        }
        let path = captured.path().as_str();
        let stub_path = path.strip_suffix(".pyi");
        let Some(module_path) = stub_path.or_else(|| path.strip_suffix(".py")) else {
            continue;
        };
        let folder = module_path.split('/').next()?;
        // A selected runtime module of the same name does not establish a stub-only layout.
        if roots.contains(folder) {
            return None;
        }
        let Some(logical) = folder.strip_suffix(STUB_SUFFIX) else {
            continue;
        };
        if !roots.contains(logical) {
            continue;
        }
        if stub_path.is_none() || !path.contains('/') || !recorded.contains(path) {
            return None;
        }
        witnessed.insert(logical);
    }
    if witnessed.len() != roots.len() {
        return None;
    }
    PackageImportRoot::new(
        None,
        roots.into_iter().map(str::to_owned).collect(),
        PackageImportRootOrigin::Wheel,
    )
    .ok()
}

fn captured_metadata<'input>(
    input: &NamespaceInput<'input>,
    directory: &str,
    name: &str,
) -> Option<&'input str> {
    let mut matches = input
        .files()
        .iter()
        .chain(input.context_sources())
        .filter(|source| {
            source
                .path()
                .as_str()
                .split_once('/')
                .is_some_and(|(parent, child)| {
                    parent.eq_ignore_ascii_case(directory) && child == name
                })
        });
    let source = matches.next()?;
    (matches.next().is_none() && source.text().len() <= input.syntax().source_bytes_max())
        .then_some(source.text())
}

fn field<'text>(metadata: &'text str, name: &str) -> Option<&'text str> {
    let mut fields = metadata
        .lines()
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.split_once(':'))
        .filter_map(|(key, value)| key.eq_ignore_ascii_case(name).then_some(value.trim()));
    let value = fields.next()?;
    (!value.is_empty() && fields.next().is_none()).then_some(value)
}
