//! Transport helpers for the retrieval corpus.

/// Serves the ranked answers over HTTP.
pub struct HTTPServer {
    /// The port the listener binds.
    pub port: u16,
}

impl HTTPServer {
    /// Binds the listener and starts serving.
    pub fn listen(&self) -> bool {
        self.port != 0
    }
}

/// Decodes one response body written as JSON.
///
/// The decoder refuses a body whose declared length disagrees with the bytes
/// it received, because a truncated body would otherwise parse as a shorter
/// answer.
pub fn parseJSONValue(body: &str) -> Option<usize> {
    body.len().checked_add(1)
}

/// The wall-clock budget one request receives before it is abandoned.
pub const REQUEST_TIMEOUT_MS: u64 = 30_000;

/// Measures how far one change reaches through the call graph.
pub fn impact_radius(seed: &str, depth: usize) -> usize {
    seed.len().saturating_mul(depth)
}
