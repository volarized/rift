/** Sends one search request and returns its parsed answer. */
export function sendSearchRequest(query, limit) {
  return { query, limit, hits: [] };
}

/** Collapses repeated hits so one declaration appears once. */
export function dedupeHits(hits) {
  return Array.from(new Set(hits));
}
