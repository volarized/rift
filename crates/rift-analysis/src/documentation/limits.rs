//! Accepted bounds for documentation collection.

use rift_error::{RiftError, errors};
use rift_protocol::documentation::DocumentationConfiguration;
use serde::Serialize;

/// Accepted documentation bounds, independent of source selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
// Bound field names match SyntaxLimits.
#[allow(clippy::struct_field_names)]
pub struct DocumentationLimits {
    pub(super) sources_max: u32,
    pub(super) source_bytes_max: u64,
    pub(super) total_bytes_max: u64,
    pub(super) blocks_max: u32,
    pub(super) references_max: u32,
    pub(super) text_bytes_max: u64,
    pub(super) heading_depth_max: u32,
    pub(super) warnings_max: u32,
    pub(super) nodes_max: u32,
    pub(super) depth_max: u32,
    pub(super) progress_callbacks_max: u32,
    pub(super) layer_blocks_max: u32,
    pub(super) mappings_max: u32,
}

impl Default for DocumentationLimits {
    fn default() -> Self {
        Self::declared(&DocumentationConfiguration::default())
    }
}

impl DocumentationLimits {
    /// Accepts collection bounds from the `[documentation]` table.
    ///
    /// # Errors
    ///
    /// Returns a registered error for a configuration outside its supported bounds.
    pub fn from_configuration(
        configuration: &DocumentationConfiguration,
    ) -> Result<Self, RiftError> {
        configuration
            .validate()
            .map_err(|violation| rift_core::configuration_violation_error(&violation))?;
        Ok(Self::declared(configuration))
    }

    fn declared(configuration: &DocumentationConfiguration) -> Self {
        Self {
            sources_max: configuration.max_sources,
            source_bytes_max: configuration.max_file.bytes(),
            total_bytes_max: configuration.max_total.bytes(),
            blocks_max: configuration.max_blocks,
            references_max: configuration.max_references,
            text_bytes_max: configuration.max_text.bytes(),
            heading_depth_max: configuration.max_heading_depth,
            warnings_max: configuration.max_warnings,
            nodes_max: configuration.max_nodes,
            depth_max: configuration.max_depth,
            progress_callbacks_max: configuration.max_progress,
            layer_blocks_max: configuration.max_layer_blocks,
            mappings_max: configuration.max_mappings,
        }
    }

    /// Checks the selected-source count before admitting another source.
    ///
    /// # Errors
    ///
    /// Returns a registered error when `count` exceeds the accepted source count.
    pub fn check_source_count(self, count: usize) -> Result<(), RiftError> {
        if count > self.sources_max as usize {
            return errors::analysis::documentation_limit_exceeded()
                .field("sources")
                .fail();
        }
        Ok(())
    }

    /// Maximum selected documentation sources.
    #[must_use]
    pub const fn sources_max(self) -> u32 {
        self.sources_max
    }

    /// Maximum bytes accepted from one documentation source.
    #[must_use]
    pub const fn source_bytes_max(self) -> u64 {
        self.source_bytes_max
    }

    /// Maximum bytes accepted across selected documentation sources.
    #[must_use]
    pub const fn total_bytes_max(self) -> u64 {
        self.total_bytes_max
    }

    #[cfg(feature = "collector")]
    pub(super) fn syntax(self) -> Result<rift_syntax::SyntaxLimits, RiftError> {
        let source_bytes_max = usize::try_from(self.source_bytes_max).map_err(|_| {
            errors::analysis::documentation_limit_exceeded()
                .field("source_bytes")
                .error()
        })?;
        rift_syntax::SyntaxLimits::new(
            source_bytes_max,
            self.nodes_max as usize,
            self.depth_max as usize,
        )
    }
}
