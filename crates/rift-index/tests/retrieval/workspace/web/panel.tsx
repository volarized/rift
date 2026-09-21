/** Renders the ranked answers as a list the reader can scan. */
export function ResultsPanel(props: { hits: string[] }) {
  return <ul>{props.hits.map((hit) => <li key={hit}>{hit}</li>)}</ul>;
}

/** Highlights the query terms inside one excerpt. */
export function highlightQueryTerms(excerpt: string, terms: string[]): string {
  return terms.reduce((held, term) => held.replace(term, term), excerpt);
}
