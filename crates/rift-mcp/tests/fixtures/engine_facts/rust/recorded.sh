# Replays answers recorded from rust-analyzer 1.98 for the fixture this suite serves.
# Generated: one arm per distinct answer, keyed by method, document and position.
frame() { printf 'Content-Length: %d\r\n\r\n%s' "${#1}" "$1"; }
while IFS= read -r header; do
  IFS= read -r blank
  length=${header%?}
  length=${length##*[!0-9]}
  body=$(dd bs=1 count="$length" 2>/dev/null)
  case "$body" in
    *'"method":"'*) ;;
    *) continue ;;
  esac
  rest=${body#*\"method\":\"}
  method=${rest%%\"*}
  case "$body" in
    '{"jsonrpc":"2.0","id":'*) rest=${body#*\"id\":}; id=${rest%%,*} ;;
    *) id= ;;
  esac
  if [ "$method" = initialize ]; then
    rest=${body#*\"rootUri\":\"}
    root=${rest%%\"*}
    frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"capabilities\":{\"positionEncoding\":\"utf-8\",\"definitionProvider\":true,\"typeDefinitionProvider\":true,\"implementationProvider\":true,\"referencesProvider\":true,\"callHierarchyProvider\":true}}}"
    frame '{"jsonrpc":"2.0","method":"$/progress","params":{"token":"replay/analysis","value":{"kind":"begin","title":"analysis"}}}'
    frame '{"jsonrpc":"2.0","method":"$/progress","params":{"token":"replay/analysis","value":{"kind":"end"}}}'
    continue
  fi
  [ "$method" = exit ] && exit 0
  [ -z "$id" ] && continue
  rest=${body#*\"uri\":\"}
  uri=${rest%%\"*}
  case "$body" in
    *'"selectionRange":{"start":{"line":'*) rest=${body#*\"selectionRange\":\{\"start\":\{\"line\":} ;;
    *) rest=${body#*\"position\":\{\"line\":} ;;
  esac
  line=${rest%%,*}
  rest=${rest#*\"character\":}
  character=${rest%%\}*}
  case "$method|${uri#"$root"/}|$line,$character" in
    'callHierarchy/outgoingCalls|src/lib.rs|1,7' | 'callHierarchy/outgoingCalls|src/lib.rs|15,7' | 'callHierarchy/outgoingCalls|src/lib.rs|21,7' | 'callHierarchy/outgoingCalls|src/lib.rs|26,7' | 'callHierarchy/outgoingCalls|src/lib.rs|9,7' | 'textDocument/prepareCallHierarchy|src/lib.rs|0,10' | 'textDocument/prepareCallHierarchy|src/lib.rs|14,15' | 'textDocument/prepareCallHierarchy|src/lib.rs|20,17' | 'textDocument/prepareCallHierarchy|src/lib.rs|4,11' | 'textDocument/prepareCallHierarchy|src/lib.rs|6,11' | 'textDocument/prepareCallHierarchy|src/lib.rs|8,15' | 'textDocument/typeDefinition|src/lib.rs|0,10' | 'textDocument/typeDefinition|src/lib.rs|1,7' | 'textDocument/typeDefinition|src/lib.rs|15,7' | 'textDocument/typeDefinition|src/lib.rs|30,7' | 'textDocument/typeDefinition|src/lib.rs|4,11' | 'textDocument/typeDefinition|src/lib.rs|40,7' | 'textDocument/typeDefinition|src/lib.rs|44,7' | 'textDocument/typeDefinition|src/lib.rs|6,11' | 'textDocument/typeDefinition|src/lib.rs|9,7')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[]}" ;;
    'callHierarchy/outgoingCalls|src/lib.rs|30,7')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"to\":{\"name\":\"default\",\"kind\":12,\"detail\":\"pub fn default() -> Self\",\"uri\":\"file:///recorded/outside/src/default.rs\",\"range\":{\"start\":{\"line\":107,\"character\":4},\"end\":{\"line\":138,\"character\":25}},\"selectionRange\":{\"start\":{\"line\":138,\"character\":7},\"end\":{\"line\":138,\"character\":14}}},\"fromRanges\":[{\"start\":{\"line\":31,\"character\":16},\"end\":{\"line\":31,\"character\":31}}]},{\"to\":{\"name\":\"area\",\"kind\":12,\"detail\":\"fn area(&self) -> f64\",\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":9,\"character\":4},\"end\":{\"line\":11,\"character\":5}},\"selectionRange\":{\"start\":{\"line\":9,\"character\":7},\"end\":{\"line\":9,\"character\":11}}},\"fromRanges\":[{\"start\":{\"line\":32,\"character\":10},\"end\":{\"line\":32,\"character\":14}}]}]}" ;;
    'callHierarchy/outgoingCalls|src/lib.rs|35,7')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"to\":{\"name\":\"default\",\"kind\":12,\"detail\":\"pub fn default() -> Self\",\"uri\":\"file:///recorded/outside/src/default.rs\",\"range\":{\"start\":{\"line\":107,\"character\":4},\"end\":{\"line\":138,\"character\":25}},\"selectionRange\":{\"start\":{\"line\":138,\"character\":7},\"end\":{\"line\":138,\"character\":14}}},\"fromRanges\":[{\"start\":{\"line\":36,\"character\":24},\"end\":{\"line\":36,\"character\":40}}]},{\"to\":{\"name\":\"area\",\"kind\":12,\"detail\":\"fn area(&self) -> f64\",\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":9,\"character\":4},\"end\":{\"line\":11,\"character\":5}},\"selectionRange\":{\"start\":{\"line\":9,\"character\":7},\"end\":{\"line\":9,\"character\":11}}},\"fromRanges\":[{\"start\":{\"line\":37,\"character\":10},\"end\":{\"line\":37,\"character\":14}}]}]}" ;;
    'callHierarchy/outgoingCalls|src/lib.rs|40,7')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"to\":{\"name\":\"sum\",\"kind\":12,\"detail\":\"pub fn sum<S>(self) -> S\\nwhere\\n    Self: Sized,\\n    S: Sum<Self::Item>,\",\"uri\":\"file:///recorded/outside/traits/iterator.rs\",\"range\":{\"start\":{\"line\":3639,\"character\":4},\"end\":{\"line\":3674,\"character\":5}},\"selectionRange\":{\"start\":{\"line\":3668,\"character\":7},\"end\":{\"line\":3668,\"character\":10}}},\"fromRanges\":[{\"start\":{\"line\":41,\"character\":45},\"end\":{\"line\":41,\"character\":48}}]},{\"to\":{\"name\":\"map\",\"kind\":12,\"detail\":\"pub fn map<B, F>(self, f: F) -> Map<Self, F>\\nwhere\\n    Self: Sized,\\n    F: FnMut(Self::Item) -> B,\",\"uri\":\"file:///recorded/outside/traits/iterator.rs\",\"range\":{\"start\":{\"line\":779,\"character\":4},\"end\":{\"line\":836,\"character\":5}},\"selectionRange\":{\"start\":{\"line\":830,\"character\":7},\"end\":{\"line\":830,\"character\":10}}},\"fromRanges\":[{\"start\":{\"line\":41,\"character\":19},\"end\":{\"line\":41,\"character\":22}}]},{\"to\":{\"name\":\"iter\",\"kind\":12,\"detail\":\"pub const fn iter(&self) -> Iter<'_, T>\",\"uri\":\"file:///recorded/outside/slice/mod.rs\",\"range\":{\"start\":{\"line\":1023,\"character\":4},\"end\":{\"line\":1044,\"character\":5}},\"selectionRange\":{\"start\":{\"line\":1042,\"character\":17},\"end\":{\"line\":1042,\"character\":21}}},\"fromRanges\":[{\"start\":{\"line\":41,\"character\":12},\"end\":{\"line\":41,\"character\":16}}]},{\"to\":{\"name\":\"boxed\",\"kind\":12,\"detail\":\"pub fn boxed() -> Vec<Circle>\",\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":26,\"character\":0},\"end\":{\"line\":28,\"character\":1}},\"selectionRange\":{\"start\":{\"line\":26,\"character\":7},\"end\":{\"line\":26,\"character\":12}}},\"fromRanges\":[{\"start\":{\"line\":41,\"character\":4},\"end\":{\"line\":41,\"character\":9}}]},{\"to\":{\"name\":\"area\",\"kind\":12,\"detail\":\"fn area(&self) -> f64\",\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":15,\"character\":4},\"end\":{\"line\":17,\"character\":5}},\"selectionRange\":{\"start\":{\"line\":15,\"character\":7},\"end\":{\"line\":15,\"character\":11}}},\"fromRanges\":[{\"start\":{\"line\":41,\"character\":37},\"end\":{\"line\":41,\"character\":41}}]}]}" ;;
    'callHierarchy/outgoingCalls|src/lib.rs|44,7')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"to\":{\"name\":\"map_or\",\"kind\":12,\"detail\":\"pub const fn map_or<U, F>(self, default: U, f: F) -> U\\nwhere\\n    F: FnOnce(T) -> U + Destruct,\\n    U: Destruct,\",\"uri\":\"file:///recorded/outside/src/option.rs\",\"range\":{\"start\":{\"line\":1199,\"character\":4},\"end\":{\"line\":1230,\"character\":5}},\"selectionRange\":{\"start\":{\"line\":1221,\"character\":17},\"end\":{\"line\":1221,\"character\":23}}},\"fromRanges\":[{\"start\":{\"line\":45,\"character\":31},\"end\":{\"line\":45,\"character\":37}}]},{\"to\":{\"name\":\"next\",\"kind\":12,\"detail\":\"fn next(&mut self) -> Option<T>\",\"uri\":\"file:///recorded/outside/vec/into_iter.rs\",\"range\":{\"start\":{\"line\":250,\"character\":4},\"end\":{\"line\":269,\"character\":5}},\"selectionRange\":{\"start\":{\"line\":251,\"character\":7},\"end\":{\"line\":251,\"character\":11}}},\"fromRanges\":[{\"start\":{\"line\":45,\"character\":24},\"end\":{\"line\":45,\"character\":28}}]},{\"to\":{\"name\":\"into_iter\",\"kind\":12,\"detail\":\"fn into_iter(self) -> Self::IntoIter\",\"uri\":\"file:///recorded/outside/vec/mod.rs\",\"range\":{\"start\":{\"line\":3949,\"character\":4},\"end\":{\"line\":3980,\"character\":5}},\"selectionRange\":{\"start\":{\"line\":3966,\"character\":7},\"end\":{\"line\":3966,\"character\":16}}},\"fromRanges\":[{\"start\":{\"line\":45,\"character\":12},\"end\":{\"line\":45,\"character\":21}}]},{\"to\":{\"name\":\"boxed\",\"kind\":12,\"detail\":\"pub fn boxed() -> Vec<Circle>\",\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":26,\"character\":0},\"end\":{\"line\":28,\"character\":1}},\"selectionRange\":{\"start\":{\"line\":26,\"character\":7},\"end\":{\"line\":26,\"character\":12}}},\"fromRanges\":[{\"start\":{\"line\":45,\"character\":4},\"end\":{\"line\":45,\"character\":9}}]},{\"to\":{\"name\":\"area\",\"kind\":12,\"detail\":\"fn area(&self) -> f64\",\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":15,\"character\":4},\"end\":{\"line\":17,\"character\":5}},\"selectionRange\":{\"start\":{\"line\":15,\"character\":7},\"end\":{\"line\":15,\"character\":11}}},\"fromRanges\":[{\"start\":{\"line\":45,\"character\":57},\"end\":{\"line\":45,\"character\":61}}]}]}" ;;
    'textDocument/definition|src/lib.rs|0,10' | 'textDocument/definition|src/lib.rs|14,5' | 'textDocument/definition|src/lib.rs|8,5')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":0,\"character\":10},\"end\":{\"line\":0,\"character\":15}}}]}" ;;
    'textDocument/definition|src/lib.rs|1,7')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":1,\"character\":7},\"end\":{\"line\":1,\"character\":11}}}]}" ;;
    'textDocument/definition|src/lib.rs|14,15' | 'textDocument/definition|src/lib.rs|26,22' | 'textDocument/definition|src/lib.rs|27,9' | 'textDocument/definition|src/lib.rs|6,11')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":6,\"character\":11},\"end\":{\"line\":6,\"character\":17}}}]}" ;;
    'textDocument/definition|src/lib.rs|15,7' | 'textDocument/definition|src/lib.rs|41,37' | 'textDocument/definition|src/lib.rs|41,40' | 'textDocument/definition|src/lib.rs|45,57' | 'textDocument/definition|src/lib.rs|45,60')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":15,\"character\":7},\"end\":{\"line\":15,\"character\":11}}}]}" ;;
    'textDocument/definition|src/lib.rs|20,17' | 'textDocument/definition|src/lib.rs|22,8' | 'textDocument/definition|src/lib.rs|31,16' | 'textDocument/definition|src/lib.rs|36,15' | 'textDocument/definition|src/lib.rs|4,11' | 'textDocument/definition|src/lib.rs|8,15')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":4,\"character\":11},\"end\":{\"line\":4,\"character\":17}}}]}" ;;
    'textDocument/definition|src/lib.rs|20,5')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"file:///recorded/outside/src/default.rs\",\"range\":{\"start\":{\"line\":106,\"character\":16},\"end\":{\"line\":106,\"character\":23}}}]}" ;;
    'textDocument/definition|src/lib.rs|21,20')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":20,\"character\":17},\"end\":{\"line\":20,\"character\":23}}}]}" ;;
    'textDocument/definition|src/lib.rs|21,7' | 'textDocument/definition|src/lib.rs|31,24' | 'textDocument/definition|src/lib.rs|31,30' | 'textDocument/definition|src/lib.rs|36,33' | 'textDocument/definition|src/lib.rs|36,39')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":21,\"character\":7},\"end\":{\"line\":21,\"character\":14}}}]}" ;;
    'textDocument/definition|src/lib.rs|26,18')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"file:///recorded/outside/vec/mod.rs\",\"range\":{\"start\":{\"line\":435,\"character\":11},\"end\":{\"line\":435,\"character\":14}}}]}" ;;
    'textDocument/definition|src/lib.rs|26,7' | 'textDocument/definition|src/lib.rs|41,4' | 'textDocument/definition|src/lib.rs|41,8' | 'textDocument/definition|src/lib.rs|45,4' | 'textDocument/definition|src/lib.rs|45,8')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":26,\"character\":7},\"end\":{\"line\":26,\"character\":12}}}]}" ;;
    'textDocument/definition|src/lib.rs|30,7' | 'textDocument/references|src/lib.rs|30,7')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":30,\"character\":7},\"end\":{\"line\":30,\"character\":12}}}]}" ;;
    'textDocument/definition|src/lib.rs|32,10' | 'textDocument/definition|src/lib.rs|32,13' | 'textDocument/definition|src/lib.rs|37,10' | 'textDocument/definition|src/lib.rs|37,13' | 'textDocument/definition|src/lib.rs|9,7')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":9,\"character\":7},\"end\":{\"line\":9,\"character\":11}}}]}" ;;
    'textDocument/definition|src/lib.rs|35,7' | 'textDocument/references|src/lib.rs|35,7')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":35,\"character\":7},\"end\":{\"line\":35,\"character\":15}}}]}" ;;
    'textDocument/definition|src/lib.rs|40,7' | 'textDocument/references|src/lib.rs|40,7')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":40,\"character\":7},\"end\":{\"line\":40,\"character\":11}}}]}" ;;
    'textDocument/definition|src/lib.rs|41,15')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"file:///recorded/outside/slice/mod.rs\",\"range\":{\"start\":{\"line\":1042,\"character\":17},\"end\":{\"line\":1042,\"character\":21}}}]}" ;;
    'textDocument/definition|src/lib.rs|41,21')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"file:///recorded/outside/traits/iterator.rs\",\"range\":{\"start\":{\"line\":830,\"character\":7},\"end\":{\"line\":830,\"character\":10}}}]}" ;;
    'textDocument/definition|src/lib.rs|41,47')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"file:///recorded/outside/traits/iterator.rs\",\"range\":{\"start\":{\"line\":3668,\"character\":7},\"end\":{\"line\":3668,\"character\":10}}}]}" ;;
    'textDocument/definition|src/lib.rs|44,7' | 'textDocument/references|src/lib.rs|44,7')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":44,\"character\":7},\"end\":{\"line\":44,\"character\":12}}}]}" ;;
    'textDocument/definition|src/lib.rs|45,20')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"file:///recorded/outside/vec/mod.rs\",\"range\":{\"start\":{\"line\":3966,\"character\":7},\"end\":{\"line\":3966,\"character\":16}}}]}" ;;
    'textDocument/definition|src/lib.rs|45,27')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"file:///recorded/outside/vec/into_iter.rs\",\"range\":{\"start\":{\"line\":251,\"character\":7},\"end\":{\"line\":251,\"character\":11}}}]}" ;;
    'textDocument/definition|src/lib.rs|45,36')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"file:///recorded/outside/src/option.rs\",\"range\":{\"start\":{\"line\":1221,\"character\":17},\"end\":{\"line\":1221,\"character\":23}}}]}" ;;
    'textDocument/implementation|src/lib.rs|0,10')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":14,\"character\":15},\"end\":{\"line\":14,\"character\":21}}},{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":8,\"character\":15},\"end\":{\"line\":8,\"character\":21}}}]}" ;;
    'textDocument/prepareCallHierarchy|src/lib.rs|1,7')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"name\":\"area\",\"kind\":12,\"detail\":\"pub fn area(&self) -> f64\",\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":1,\"character\":4},\"end\":{\"line\":1,\"character\":26}},\"selectionRange\":{\"start\":{\"line\":1,\"character\":7},\"end\":{\"line\":1,\"character\":11}}}]}" ;;
    'textDocument/prepareCallHierarchy|src/lib.rs|15,7')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"name\":\"area\",\"kind\":12,\"detail\":\"fn area(&self) -> f64\",\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":15,\"character\":4},\"end\":{\"line\":17,\"character\":5}},\"selectionRange\":{\"start\":{\"line\":15,\"character\":7},\"end\":{\"line\":15,\"character\":11}}}]}" ;;
    'textDocument/prepareCallHierarchy|src/lib.rs|21,7')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"name\":\"default\",\"kind\":12,\"detail\":\"fn default() -> Self\",\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":21,\"character\":4},\"end\":{\"line\":23,\"character\":5}},\"selectionRange\":{\"start\":{\"line\":21,\"character\":7},\"end\":{\"line\":21,\"character\":14}}}]}" ;;
    'textDocument/prepareCallHierarchy|src/lib.rs|26,7')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"name\":\"boxed\",\"kind\":12,\"detail\":\"pub fn boxed() -> Vec<Circle>\",\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":26,\"character\":0},\"end\":{\"line\":28,\"character\":1}},\"selectionRange\":{\"start\":{\"line\":26,\"character\":7},\"end\":{\"line\":26,\"character\":12}}}]}" ;;
    'textDocument/prepareCallHierarchy|src/lib.rs|30,7')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"name\":\"total\",\"kind\":12,\"detail\":\"pub fn total() -> f64\",\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":30,\"character\":0},\"end\":{\"line\":33,\"character\":1}},\"selectionRange\":{\"start\":{\"line\":30,\"character\":7},\"end\":{\"line\":30,\"character\":12}}}]}" ;;
    'textDocument/prepareCallHierarchy|src/lib.rs|35,7')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"name\":\"inferred\",\"kind\":12,\"detail\":\"pub fn inferred() -> f64\",\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":35,\"character\":0},\"end\":{\"line\":38,\"character\":1}},\"selectionRange\":{\"start\":{\"line\":35,\"character\":7},\"end\":{\"line\":35,\"character\":15}}}]}" ;;
    'textDocument/prepareCallHierarchy|src/lib.rs|40,7')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"name\":\"each\",\"kind\":12,\"detail\":\"pub fn each() -> f64\",\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":40,\"character\":0},\"end\":{\"line\":42,\"character\":1}},\"selectionRange\":{\"start\":{\"line\":40,\"character\":7},\"end\":{\"line\":40,\"character\":11}}}]}" ;;
    'textDocument/prepareCallHierarchy|src/lib.rs|44,7')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"name\":\"first\",\"kind\":12,\"detail\":\"pub fn first() -> f64\",\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":44,\"character\":0},\"end\":{\"line\":46,\"character\":1}},\"selectionRange\":{\"start\":{\"line\":44,\"character\":7},\"end\":{\"line\":44,\"character\":12}}}]}" ;;
    'textDocument/prepareCallHierarchy|src/lib.rs|9,7')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"name\":\"area\",\"kind\":12,\"detail\":\"fn area(&self) -> f64\",\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":9,\"character\":4},\"end\":{\"line\":11,\"character\":5}},\"selectionRange\":{\"start\":{\"line\":9,\"character\":7},\"end\":{\"line\":9,\"character\":11}}}]}" ;;
    'textDocument/references|src/lib.rs|0,10')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":8,\"character\":5},\"end\":{\"line\":8,\"character\":10}}},{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":14,\"character\":5},\"end\":{\"line\":14,\"character\":10}}},{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":0,\"character\":10},\"end\":{\"line\":0,\"character\":15}}}]}" ;;
    'textDocument/references|src/lib.rs|1,7')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":9,\"character\":7},\"end\":{\"line\":9,\"character\":11}}},{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":15,\"character\":7},\"end\":{\"line\":15,\"character\":11}}},{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":32,\"character\":10},\"end\":{\"line\":32,\"character\":14}}},{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":37,\"character\":10},\"end\":{\"line\":37,\"character\":14}}},{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":41,\"character\":37},\"end\":{\"line\":41,\"character\":41}}},{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":45,\"character\":57},\"end\":{\"line\":45,\"character\":61}}},{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":1,\"character\":7},\"end\":{\"line\":1,\"character\":11}}}]}" ;;
    'textDocument/references|src/lib.rs|14,15' | 'textDocument/references|src/lib.rs|6,11')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":14,\"character\":15},\"end\":{\"line\":14,\"character\":21}}},{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":26,\"character\":22},\"end\":{\"line\":26,\"character\":28}}},{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":27,\"character\":9},\"end\":{\"line\":27,\"character\":15}}},{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":6,\"character\":11},\"end\":{\"line\":6,\"character\":17}}}]}" ;;
    'textDocument/references|src/lib.rs|15,7')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":41,\"character\":37},\"end\":{\"line\":41,\"character\":41}}},{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":45,\"character\":57},\"end\":{\"line\":45,\"character\":61}}},{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":15,\"character\":7},\"end\":{\"line\":15,\"character\":11}}}]}" ;;
    'textDocument/references|src/lib.rs|20,17' | 'textDocument/references|src/lib.rs|4,11' | 'textDocument/references|src/lib.rs|8,15')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":8,\"character\":15},\"end\":{\"line\":8,\"character\":21}}},{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":20,\"character\":17},\"end\":{\"line\":20,\"character\":23}}},{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":22,\"character\":8},\"end\":{\"line\":22,\"character\":14}}},{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":31,\"character\":16},\"end\":{\"line\":31,\"character\":22}}},{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":36,\"character\":15},\"end\":{\"line\":36,\"character\":21}}},{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":21,\"character\":20},\"end\":{\"line\":21,\"character\":24}}},{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":4,\"character\":11},\"end\":{\"line\":4,\"character\":17}}}]}" ;;
    'textDocument/references|src/lib.rs|21,7')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":31,\"character\":24},\"end\":{\"line\":31,\"character\":31}}},{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":36,\"character\":33},\"end\":{\"line\":36,\"character\":40}}},{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":21,\"character\":7},\"end\":{\"line\":21,\"character\":14}}}]}" ;;
    'textDocument/references|src/lib.rs|26,7')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":41,\"character\":4},\"end\":{\"line\":41,\"character\":9}}},{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":45,\"character\":4},\"end\":{\"line\":45,\"character\":9}}},{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":26,\"character\":7},\"end\":{\"line\":26,\"character\":12}}}]}" ;;
    'textDocument/references|src/lib.rs|9,7')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":32,\"character\":10},\"end\":{\"line\":32,\"character\":14}}},{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":37,\"character\":10},\"end\":{\"line\":37,\"character\":14}}},{\"uri\":\"$root/src/lib.rs\",\"range\":{\"start\":{\"line\":9,\"character\":7},\"end\":{\"line\":9,\"character\":11}}}]}" ;;
    *) frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":null}" ;;
  esac
done
