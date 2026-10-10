//! Pattern search over the source of selected exact package versions.

use rift_error::RiftError;
use std::collections::HashSet;

use rift_protocol::read::SEARCH_PATTERN_CHARS_MAX;

use crate::{
    Capabilities, GlobalClient, HitLocation, PackagePatternHit, PackagePatternPage,
    PackagePatternRequest, PageMetadata, SearchPackagePatternsRequestQuery,
    active_response_body_bytes_max, advertised_page_limit, bounded_nonempty_characters, contract,
    failure, includes_source, package_key, package_source_path, response, serialize_body,
    smaller_bound, supports_feature, validate_body_for_capabilities, validate_hit_common,
    validate_packages, validate_page, validate_read_bounds,
};

/// Most distinct files one pattern page holds matches from. The server verifies candidate files
/// in path order and stops a page at this many, so `next_cursor` continues from the next file.
pub const PATTERN_PAGE_FILES_MAX: usize = 500;

/// The capability feature a server advertises when it serves pattern search.
const PATTERN_FEATURE: &str = "patterns";

impl GlobalClient {
    /// Reads one page of pattern matches over the selected exact package versions, asking for
    /// the smaller of `limit` and the advertised `page_limit_max`.
    ///
    /// The page stops at that many matches, at [`PATTERN_PAGE_FILES_MAX`] files, or at the text
    /// bound the server verifies per page, and a page that stopped before the last match
    /// carries a `result_truncated` warning; the caller decides whether to follow
    /// `next_cursor`.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the request or answer breaks the contract, the server does
    /// not advertise pattern search, or the endpoint is unavailable.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future abandons the request. Capabilities already read stay cached, and the
    /// abandoned request records no failure.
    pub async fn search_package_patterns(
        &self,
        request: &PackagePatternRequest,
        limit: i64,
        cursor: Option<&str>,
    ) -> Result<PackagePatternPage, RiftError> {
        rift_tracing::traced!(
            component = "global",
            operation = "search_package_patterns",
            async {
                if !self.inner.enabled {
                    return Err(failure::configuration_invalid(
                        "enabled",
                        "global client disabled",
                    ));
                }
                validate_pattern_request(request)?;
                validate_page(limit, cursor)?;
                let capabilities = self.get_capabilities().await?;
                let limit = advertised_page_limit(limit, &capabilities);
                validate_pattern_request_for_capabilities(request, limit, cursor, &capabilities)?;
                let body = serialize_body(request, self.inner.config.max_request)?;
                validate_body_for_capabilities(
                    &body,
                    &capabilities,
                    self.inner.config.max_request,
                )?;
                let query = SearchPackagePatternsRequestQuery {
                    limit: Some(limit),
                    cursor: cursor.map(str::to_owned),
                };
                let response_body_bytes_max =
                    active_response_body_bytes_max(&capabilities, self.inner.config.max_response);
                let raw = self
                    .request_with_query(
                        contract::Endpoint::Patterns,
                        body,
                        &query,
                        response_body_bytes_max,
                    )
                    .await;
                let raw = self.observed(raw).await?;
                let page = response::patterns(raw).await.map(|parsed| parsed.value);
                let page = self.observed(page).await?;
                let checked = PatternPageCheck {
                    request,
                    capabilities: &capabilities,
                    limit,
                    cursor,
                    source_bytes_max: self.inner.config.max_source,
                }
                .validate(&page);
                self.observed(checked).await?;
                Ok(page)
            }
        )
        .await
    }
}

pub(crate) fn validate_pattern_request(request: &PackagePatternRequest) -> Result<(), RiftError> {
    bounded_nonempty_characters(&request.pattern, SEARCH_PATTERN_CHARS_MAX, "pattern")?;
    if request
        .include
        .as_ref()
        .is_some_and(|values| values.len() > 1 || values.iter().any(|value| value != "source"))
    {
        return Err(failure::request_invalid(
            "validate_pattern_request",
            "include",
            "field failed validation",
        ));
    }
    validate_packages(request.packages.as_deref().unwrap_or_default())
}

pub(crate) fn validate_pattern_request_for_capabilities(
    request: &PackagePatternRequest,
    limit: i64,
    cursor: Option<&str>,
    capabilities: &Capabilities,
) -> Result<(), RiftError> {
    if !supports_feature(capabilities, PATTERN_FEATURE) {
        return Err(failure::configuration_invalid(
            "feature",
            &format!("global API does not advertise feature: {PATTERN_FEATURE}"),
        ));
    }
    validate_read_bounds(
        request.packages.as_ref().map_or(0, Vec::len),
        limit,
        cursor,
        capabilities,
    )
}

/// What one pattern page is checked against: the request that asked for it, under the
/// capabilities and page arguments it was asked with.
pub(crate) struct PatternPageCheck<'a> {
    pub(crate) request: &'a PackagePatternRequest,
    pub(crate) capabilities: &'a Capabilities,
    pub(crate) limit: i64,
    pub(crate) cursor: Option<&'a str>,
    pub(crate) source_bytes_max: usize,
}

impl PatternPageCheck<'_> {
    pub(crate) fn validate(&self, page: &PackagePatternPage) -> Result<(), RiftError> {
        PageMetadata {
            publication_format: &page.publication_format,
            analyzer_revision: &page.analyzer_revision,
            corpus_revision: &page.corpus_revision,
            documentation_revision: None,
            next_cursor: page.next_cursor.as_deref(),
            warnings: &page.warnings,
        }
        .validate(self.capabilities, false)?;
        let within_limit = i64::try_from(page.items.len()).is_ok_and(|count| count <= self.limit);
        if !within_limit {
            return Err(failure::response_invalid(
                "validate",
                "items",
                "field failed validation",
                None,
            ));
        }
        let packages: HashSet<_> = self
            .request
            .packages
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(package_key)
            .collect();
        let mut seen = HashSet::new();
        let mut files = HashSet::new();
        for hit in &page.items {
            self.validate_hit(hit, &packages)?;
            if !seen.insert((hit.unit.as_str(), hit.range.start, hit.range.end)) {
                return Err(failure::response_invalid(
                    "validate",
                    "duplicate_item",
                    "field failed validation",
                    None,
                ));
            }
            files.insert(hit.unit.as_str());
        }
        if files.len() > PATTERN_PAGE_FILES_MAX {
            return Err(failure::response_invalid(
                "validate",
                "files",
                "field failed validation",
                None,
            ));
        }
        if self.cursor.is_some()
            && page.items.is_empty()
            && page.next_cursor.as_deref() == self.cursor
        {
            return Err(failure::response_invalid(
                "validate",
                "cursor_progress",
                "field failed validation",
                None,
            ));
        }
        Ok(())
    }

    /// Checks one match: a file of a requested package, a well-formed location inside the file,
    /// source only when asked for, and a declaration, when named, that holds the match.
    fn validate_hit(
        &self,
        hit: &PackagePatternHit,
        packages: &HashSet<(String, String, String, String)>,
    ) -> Result<(), RiftError> {
        super::validate_package_identity(&hit.package).map_err(|source| {
            failure::response_source(
                "validate_hit",
                "package",
                "field failed validation",
                None,
                source,
            )
        })?;
        if !packages.is_empty() && !packages.contains(&package_key(&hit.package)) {
            return Err(failure::response_invalid(
                "validate_hit",
                "package",
                "field failed validation",
                None,
            ));
        }
        let inside_the_file = u64::try_from(hit.size).is_ok_and(|size| hit.range.end <= size);
        if hit.line < 1 || hit.range.end < hit.range.start || !inside_the_file {
            return Err(failure::response_invalid(
                "validate_hit",
                "location",
                "field failed validation",
                None,
            ));
        }
        let source_bytes_max = smaller_bound(
            self.capabilities.bounds.source_bytes_max,
            self.source_bytes_max,
        );
        let source_allowed = includes_source(self.request.include.as_deref());
        if hit
            .source
            .as_ref()
            .is_some_and(|source| !source_allowed || source.len() > source_bytes_max)
        {
            return Err(failure::response_invalid(
                "validate_hit",
                "source",
                "field failed validation",
                None,
            ));
        }
        package_source_path(&hit.package, &hit.unit)?;
        let Some(declaration) = &hit.declaration else {
            return Ok(());
        };
        if declaration.source.is_some() && !source_allowed {
            return Err(failure::response_invalid(
                "validate_hit",
                "source",
                "field failed validation",
                None,
            ));
        }
        let location = HitLocation {
            unit: &hit.unit,
            range: &declaration.range,
            line: declaration.line,
            source: declaration.source.as_deref(),
        };
        validate_hit_common(
            &hit.package,
            &declaration.symbol,
            location,
            (!packages.is_empty()).then_some(packages),
            source_bytes_max,
        )?;
        let holds_the_match =
            declaration.range.start <= hit.range.start && hit.range.end <= declaration.range.end;
        if !holds_the_match {
            return Err(failure::response_invalid(
                "validate_hit",
                "declaration",
                "field failed validation",
                None,
            ));
        }
        Ok(())
    }
}
