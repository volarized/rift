//! Shared request-driven language engine fixture.

/// A `sh` engine serving references and call hierarchy over `lib.rs`, whose `caller`
/// calls `beacon`. It prepares `caller` at any position, names `beacon` at line
/// `CALLEE_LINE` as the one callee, and answers references with `beacon`'s own occurrence
/// on line 0 beside a call at line `CALLER_LINE`. `PROGRESS` runs once `initialize` is
/// answered.
pub(crate) const SCRIPTED_CALLS_ENGINE: &str = r#"frame() { printf 'Content-Length: %d\r\n\r\n%s' "${#1}" "$1"; }
while IFS= read -r header; do
  IFS= read -r blank
  length=$(printf '%s' "$header" | tr -dc 0-9)
  body=$(dd bs=1 count="$length" 2>/dev/null)
  id=$(printf '%s' "$body" | grep -o '"id":[0-9]*' | head -1 | tr -dc 0-9)
  uri=$(printf '%s' "$body" | grep -o '"uri":"[^"]*"' | head -1 | cut -d'"' -f4)
  caller_uri=${CALLER_URI:-$uri}
  case "$body" in
    *'"method":"initialize"'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"capabilities\":{\"referencesProvider\":true,\"callHierarchyProvider\":true}}}"
      PROGRESS ;;
    *'"method":"textDocument/prepareCallHierarchy"'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"name\":\"caller\",\"kind\":12,\"uri\":\"$uri\",\"range\":{\"start\":{\"line\":2,\"character\":0},\"end\":{\"line\":4,\"character\":1}},\"selectionRange\":{\"start\":{\"line\":2,\"character\":7},\"end\":{\"line\":2,\"character\":13}}}]}" ;;
    *'"method":"callHierarchy/outgoingCalls"'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"to\":{\"name\":\"beacon\",\"kind\":12,\"uri\":\"$uri\",\"range\":{\"start\":{\"line\":CALLEE_LINE,\"character\":0},\"end\":{\"line\":CALLEE_LINE,\"character\":18}},\"selectionRange\":{\"start\":{\"line\":CALLEE_LINE,\"character\":7},\"end\":{\"line\":CALLEE_LINE,\"character\":13}}},\"fromRanges\":[]}]}" ;;
    *'"method":"textDocument/references"'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$uri\",\"range\":{\"start\":{\"line\":0,\"character\":7},\"end\":{\"line\":0,\"character\":13}}},{\"uri\":\"$caller_uri\",\"range\":{\"start\":{\"line\":CALLER_LINE,\"character\":4},\"end\":{\"line\":CALLER_LINE,\"character\":10}}}]}" ;;
    *'"method":"exit"'*)
      exit 0 ;;
    *'"id":'[0-9]*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":null}" ;;
  esac
done
"#;
