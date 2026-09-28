//! Pattern search over the source of selected exact package versions.

use std::collections::HashSet;

use rift_protocol::read::SEARCH_PATTERN_CHARS_MAX;

use crate::{
    Capabilities, ClientError, GlobalClient, HitLocation, PackagePatternHit, PackagePatternPage,
    PackagePatternRequest, PageMetadata, SOURCE_BYTES_MAX, SearchPackagePatternsRequestQuery,
    active_response_body_bytes_max, advertised_page_limit, bounded_nonempty, contract,
    includes_source, package_key, package_source_path, response, serialize_body, smaller_bound,
    supports_feature, validate_body_for_capabilities, validate_hit_common, validate_packages,
    validate_page, validate_read_bounds,
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
    /// Returns [`ClientError`] when the request or answer breaks the contract, the server does
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
    ) -> Result<PackagePatternPage, ClientError> {
        if !self.inner.enabled {
            return Err(ClientError::Disabled);
        }
        validate_pattern_request(request)?;
        validate_page(limit, cursor)?;
        let capabilities = self.get_capabilities().await?;
        let limit = advertised_page_limit(limit, &capabilities);
        validate_pattern_request_for_capabilities(request, limit, cursor, &capabilities)?;
        let body = serialize_body(request)?;
        validate_body_for_capabilities(&body, &capabilities)?;
        let query = SearchPackagePatternsRequestQuery {
            limit: Some(limit),
            cursor: cursor.map(str::to_owned),
        };
        let response_body_bytes_max = active_response_body_bytes_max(&capabilities);
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
        }
        .validate(&page);
        self.observed(checked).await?;
        Ok(page)
    }
}

pub(crate) fn validate_pattern_request(request: &PackagePatternRequest) -> Result<(), ClientError> {
    bounded_nonempty(&request.pattern, SEARCH_PATTERN_CHARS_MAX, "pattern")?;
    if request
        .include
        .as_ref()
        .is_some_and(|values| values.len() > 1 || values.iter().any(|value| value != "source"))
    {
        return Err(ClientError::InvalidRequest { field: "include" });
    }
    validate_packages(&request.packages)
}

pub(crate) fn validate_pattern_request_for_capabilities(
    request: &PackagePatternRequest,
    limit: i64,
    cursor: Option<&str>,
    capabilities: &Capabilities,
) -> Result<(), ClientError> {
    if !supports_feature(capabilities, PATTERN_FEATURE) {
        return Err(ClientError::InvalidRequest { field: "pattern" });
    }
    validate_read_bounds(request.packages.len(), limit, cursor, capabilities)
}

/// What one pattern page is checked against: the request that asked for it, under the
/// capabilities and page arguments it was asked with.
pub(crate) struct PatternPageCheck<'a> {
    pub(crate) request: &'a PackagePatternRequest,
    pub(crate) capabilities: &'a Capabilities,
    pub(crate) limit: i64,
    pub(crate) cursor: Option<&'a str>,
}

impl PatternPageCheck<'_> {
    pub(crate) fn validate(&self, page: &PackagePatternPage) -> Result<(), ClientError> {
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
            return Err(ClientError::InvalidResponseField { field: "items" });
        }
        let packages: HashSet<_> = self.request.packages.iter().map(package_key).collect();
        let mut seen = HashSet::new();
        let mut files = HashSet::new();
        for hit in &page.items {
            self.validate_hit(hit, &packages)?;
            if !seen.insert((hit.unit.as_str(), hit.range.start, hit.range.end)) {
                return Err(ClientError::InvalidResponseField {
                    field: "duplicate_item",
                });
            }
            files.insert(hit.unit.as_str());
        }
        if files.len() > PATTERN_PAGE_FILES_MAX {
            return Err(ClientError::InvalidResponseField { field: "files" });
        }
        if self.cursor.is_some()
            && page.items.is_empty()
            && page.next_cursor.as_deref() == self.cursor
        {
            return Err(ClientError::InvalidResponseField {
                field: "cursor_progress",
            });
        }
        Ok(())
    }

    /// Checks one match: a file of a requested package, a well-formed location inside the file,
    /// source only when asked for, and a declaration, when named, that holds the match.
    fn validate_hit(
        &self,
        hit: &PackagePatternHit,
        packages: &HashSet<(String, String, String)>,
    ) -> Result<(), ClientError> {
        if !packages.contains(&package_key(&hit.package)) {
            return Err(ClientError::InvalidResponseField { field: "package" });
        }
        let inside_the_file = u64::try_from(hit.size).is_ok_and(|size| hit.range.end <= size);
        if hit.line < 1 || hit.range.end < hit.range.start || !inside_the_file {
            return Err(ClientError::InvalidResponseField { field: "location" });
        }
        let source_bytes_max =
            smaller_bound(self.capabilities.bounds.source_bytes_max, SOURCE_BYTES_MAX);
        let source_allowed = includes_source(self.request.include.as_deref());
        if hit
            .source
            .as_ref()
            .is_some_and(|source| !source_allowed || source.len() > source_bytes_max)
        {
            return Err(ClientError::InvalidResponseField { field: "source" });
        }
        package_source_path(&hit.package, &hit.unit)?;
        let Some(declaration) = &hit.declaration else {
            return Ok(());
        };
        if declaration.source.is_some() && !source_allowed {
            return Err(ClientError::InvalidResponseField { field: "source" });
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
            packages,
            source_bytes_max,
        )?;
        let holds_the_match =
            declaration.range.start <= hit.range.start && hit.range.end <= declaration.range.end;
        if !holds_the_match {
            return Err(ClientError::InvalidResponseField {
                field: "declaration",
            });
        }
        Ok(())
    }
}
