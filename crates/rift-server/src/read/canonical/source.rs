use std::path::Path;
use std::time::SystemTime;

use rift_error::RiftError;
use rift_protocol::identity::SymbolOwner;
use rift_protocol::read::{SourceExcerpt, SourceUnitSpan, TextRange};
use rift_protocol::source_read::{GetSourceParams, GetSourceResult};
use rift_protocol::symbol_read::{CapturedView, SymbolUnavailableReason};

use super::{CapturedMetadata, LocalSymbolMaterial, LocalSymbolViews, invalid, material_error};

impl LocalSymbolViews {
    /// Reads physical source bytes from the requested retained local view.
    ///
    /// # Errors
    /// Returns registered request, byte-range or captured metadata failures.
    pub fn get_source(
        &self,
        request: &GetSourceParams,
        root: &Path,
        owner: &SymbolOwner,
    ) -> Result<GetSourceResult, RiftError> {
        if !request.is_valid() {
            return Err(invalid(
                "source",
                "source selectors or byte range are invalid",
            ));
        }
        let Some(selected) = request.view.as_ref() else {
            return Err(invalid(
                "view",
                "select the view returned by a captured read",
            ));
        };
        let held = match self.held_view_at(selected, root, owner, SystemTime::now()) {
            Ok(held) => held,
            Err(reason) => return Ok(source_unavailable(request, None, reason)),
        };
        held.material.get_source(request, &held.view)
    }
}

impl LocalSymbolMaterial {
    fn get_source(
        &self,
        request: &GetSourceParams,
        view: &CapturedView,
    ) -> Result<GetSourceResult, RiftError> {
        let (_, path) = rift_protocol::identity::parse_source_unit_identity(request.unit.as_str())?;
        let path = rift_core::ProjectPath::new(path)?;
        let metadata: CapturedMetadata = serde_json::from_slice(&self.metadata)
            .map_err(|error| material_error("decode captured metadata", &error))?;
        let text = if request.unit.is_project() {
            self.index.source(&path)
        } else {
            match self
                .package_source(&request.unit)
                .map(|source| source.text.as_deref())
            {
                Some(Some(text)) => Some(text),
                Some(None) => {
                    return Ok(source_unavailable(
                        request,
                        Some(view.clone()),
                        SymbolUnavailableReason::InsufficientCoverage,
                    ));
                }
                None => {
                    return Ok(source_unavailable(
                        request,
                        Some(view.clone()),
                        SymbolUnavailableReason::ViewContext,
                    ));
                }
            }
        };
        let Some(text) = text else {
            return Ok(GetSourceResult::Missing {
                unit: request.unit.clone(),
                view: view.clone(),
                warnings: metadata.warnings,
            });
        };
        let range = request.range.clone().unwrap_or(TextRange {
            start: 0,
            end: u64::try_from(text.len())
                .map_err(|error| material_error("source byte range", &error))?,
        });
        let start = usize::try_from(range.start)
            .map_err(|error| material_error("source byte range", &error))?;
        let end = usize::try_from(range.end)
            .map_err(|error| material_error("source byte range", &error))?;
        let excerpt = text
            .get(start..end)
            .ok_or_else(|| invalid("range", "source range must contain complete UTF-8 bytes"))?;
        Ok(GetSourceResult::Found {
            source: SourceExcerpt {
                span: SourceUnitSpan {
                    unit: request.unit.clone(),
                    range,
                },
                text: excerpt.to_owned(),
            },
            source_complete: true,
            view: view.clone(),
            warnings: metadata.warnings,
        })
    }
}

fn source_unavailable(
    request: &GetSourceParams,
    view: Option<CapturedView>,
    reason: SymbolUnavailableReason,
) -> GetSourceResult {
    GetSourceResult::Unavailable {
        unit: request.unit.clone(),
        view,
        reason,
        warnings: Vec::new(),
    }
}
