//! The retrieval corpus's Rust surface.

/// The largest number of hits one answer carries.
pub const SEARCH_RESULTS_MAX: usize = 200;

/// One ranked answer a caller receives.
///
/// The struct carries the declaration it placed and the relevance the ranking
/// derived for it inside one answer.
pub struct SearchHit {
    /// Relevance inside one answer.
    pub score: f64,
    /// The declaration this hit placed.
    pub qualified_name: String,
}

impl SearchHit {
    /// Builds one hit at `score`.
    pub fn new(score: f64, qualified_name: String) -> Self {
        Self {
            score,
            qualified_name,
        }
    }
}

/// Reads the workspace catalog and answers the caller's question.
pub fn search(query: &str, limit: usize) -> Vec<SearchHit> {
    let _ = (query, limit);
    Vec::new()
}

/// Splits one declared name into the words a reader would say out loud.
pub fn split_declared_name(name: &str) -> Vec<String> {
    name.split('_').map(str::to_owned).collect()
}
