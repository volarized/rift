# Replays answers recorded from typescript-language-server 6.1.0 with TypeScript 6.0.2 for the fixture this suite serves.
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
    frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"capabilities\":{\"definitionProvider\":true,\"referencesProvider\":true,\"implementationProvider\":true,\"typeDefinitionProvider\":true,\"callHierarchyProvider\":true}}}"
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
    'callHierarchy/outgoingCalls|src/main.ts|17,16')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"to\":{\"kind\":6,\"name\":\"area\",\"detail\":\"Circle\",\"uri\":\"$root/src/shapes.ts\",\"range\":{\"start\":{\"line\":11,\"character\":2},\"end\":{\"line\":13,\"character\":3}},\"selectionRange\":{\"start\":{\"line\":11,\"character\":2},\"end\":{\"line\":11,\"character\":6}}},\"fromRanges\":[{\"start\":{\"line\":18,\"character\":9},\"end\":{\"line\":18,\"character\":29}},{\"start\":{\"line\":18,\"character\":9},\"end\":{\"line\":18,\"character\":29}}]}]}" ;;
    'callHierarchy/outgoingCalls|src/main.ts|21,16')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"to\":{\"kind\":6,\"name\":\"map\",\"detail\":\"\",\"uri\":\"file:///recorded/outside/lib/lib.es5.d.ts\",\"range\":{\"start\":{\"line\":1464,\"character\":4},\"end\":{\"line\":1469,\"character\":87}},\"selectionRange\":{\"start\":{\"line\":1469,\"character\":4},\"end\":{\"line\":1469,\"character\":7}}},\"fromRanges\":[{\"start\":{\"line\":22,\"character\":9},\"end\":{\"line\":22,\"character\":27}},{\"start\":{\"line\":22,\"character\":9},\"end\":{\"line\":22,\"character\":27}}]},{\"to\":{\"kind\":5,\"name\":\"Circle\",\"detail\":\"\",\"uri\":\"$root/src/shapes.ts\",\"range\":{\"start\":{\"line\":10,\"character\":0},\"end\":{\"line\":14,\"character\":1}},\"selectionRange\":{\"start\":{\"line\":10,\"character\":13},\"end\":{\"line\":10,\"character\":19}}},\"fromRanges\":[{\"start\":{\"line\":22,\"character\":14},\"end\":{\"line\":22,\"character\":20}},{\"start\":{\"line\":22,\"character\":14},\"end\":{\"line\":22,\"character\":20}}]},{\"to\":{\"kind\":6,\"name\":\"area\",\"detail\":\"Circle\",\"uri\":\"$root/src/shapes.ts\",\"range\":{\"start\":{\"line\":11,\"character\":2},\"end\":{\"line\":13,\"character\":3}},\"selectionRange\":{\"start\":{\"line\":11,\"character\":2},\"end\":{\"line\":11,\"character\":6}}},\"fromRanges\":[{\"start\":{\"line\":22,\"character\":39},\"end\":{\"line\":22,\"character\":49}},{\"start\":{\"line\":22,\"character\":39},\"end\":{\"line\":22,\"character\":49}}]}]}" ;;
    'callHierarchy/outgoingCalls|src/main.ts|25,16')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"to\":{\"kind\":6,\"name\":\"toUpperCase\",\"detail\":\"\",\"uri\":\"file:///recorded/outside/lib/lib.es5.d.ts\",\"range\":{\"start\":{\"line\":506,\"character\":4},\"end\":{\"line\":507,\"character\":26}},\"selectionRange\":{\"start\":{\"line\":507,\"character\":4},\"end\":{\"line\":507,\"character\":15}}},\"fromRanges\":[{\"start\":{\"line\":26,\"character\":9},\"end\":{\"line\":26,\"character\":39}},{\"start\":{\"line\":26,\"character\":9},\"end\":{\"line\":26,\"character\":39}}]}]}" ;;
    'callHierarchy/outgoingCalls|src/main.ts|4,16')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"to\":{\"kind\":12,\"name\":\"make\",\"detail\":\"\",\"uri\":\"$root/src/shapes.ts\",\"range\":{\"start\":{\"line\":16,\"character\":0},\"end\":{\"line\":18,\"character\":1}},\"selectionRange\":{\"start\":{\"line\":16,\"character\":16},\"end\":{\"line\":16,\"character\":20}}},\"fromRanges\":[{\"start\":{\"line\":5,\"character\":16},\"end\":{\"line\":5,\"character\":20}}]},{\"to\":{\"kind\":6,\"name\":\"area\",\"detail\":\"Square\",\"uri\":\"$root/src/shapes.ts\",\"range\":{\"start\":{\"line\":5,\"character\":2},\"end\":{\"line\":7,\"character\":3}},\"selectionRange\":{\"start\":{\"line\":5,\"character\":2},\"end\":{\"line\":5,\"character\":6}}},\"fromRanges\":[{\"start\":{\"line\":6,\"character\":9},\"end\":{\"line\":6,\"character\":19}},{\"start\":{\"line\":6,\"character\":9},\"end\":{\"line\":6,\"character\":19}}]}]}" ;;
    'callHierarchy/outgoingCalls|src/main.ts|9,16')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"to\":{\"kind\":5,\"name\":\"Square\",\"detail\":\"\",\"uri\":\"$root/src/shapes.ts\",\"range\":{\"start\":{\"line\":4,\"character\":0},\"end\":{\"line\":8,\"character\":1}},\"selectionRange\":{\"start\":{\"line\":4,\"character\":13},\"end\":{\"line\":4,\"character\":19}}},\"fromRanges\":[{\"start\":{\"line\":11,\"character\":27},\"end\":{\"line\":11,\"character\":33}},{\"start\":{\"line\":11,\"character\":41},\"end\":{\"line\":11,\"character\":47}}]},{\"to\":{\"kind\":6,\"name\":\"area\",\"detail\":\"Square\",\"uri\":\"$root/src/shapes.ts\",\"range\":{\"start\":{\"line\":5,\"character\":2},\"end\":{\"line\":7,\"character\":3}},\"selectionRange\":{\"start\":{\"line\":5,\"character\":2},\"end\":{\"line\":5,\"character\":6}}},\"fromRanges\":[{\"start\":{\"line\":12,\"character\":14},\"end\":{\"line\":12,\"character\":24}},{\"start\":{\"line\":12,\"character\":14},\"end\":{\"line\":12,\"character\":24}}]}]}" ;;
    'callHierarchy/outgoingCalls|src/shapes.ts|1,2' | 'callHierarchy/outgoingCalls|src/shapes.ts|10,13' | 'callHierarchy/outgoingCalls|src/shapes.ts|11,2' | 'callHierarchy/outgoingCalls|src/shapes.ts|4,13' | 'callHierarchy/outgoingCalls|src/shapes.ts|5,2' | 'textDocument/typeDefinition|src/main.ts|10,6')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[]}" ;;
    'callHierarchy/outgoingCalls|src/shapes.ts|16,16')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"to\":{\"kind\":5,\"name\":\"Square\",\"detail\":\"\",\"uri\":\"$root/src/shapes.ts\",\"range\":{\"start\":{\"line\":4,\"character\":0},\"end\":{\"line\":8,\"character\":1}},\"selectionRange\":{\"start\":{\"line\":4,\"character\":13},\"end\":{\"line\":4,\"character\":19}}},\"fromRanges\":[{\"start\":{\"line\":17,\"character\":13},\"end\":{\"line\":17,\"character\":19}}]}]}" ;;
    'textDocument/definition|src/main.ts|0,17' | 'textDocument/definition|src/main.ts|11,27' | 'textDocument/definition|src/main.ts|11,32' | 'textDocument/definition|src/main.ts|11,41' | 'textDocument/definition|src/main.ts|11,46' | 'textDocument/definition|src/shapes.ts|17,13' | 'textDocument/definition|src/shapes.ts|17,18' | 'textDocument/definition|src/shapes.ts|4,13' | 'textDocument/typeDefinition|src/main.ts|11,13' | 'textDocument/typeDefinition|src/main.ts|2,13' | 'textDocument/typeDefinition|src/main.ts|5,8' | 'textDocument/typeDefinition|src/shapes.ts|16,16')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/shapes.ts\",\"range\":{\"start\":{\"line\":4,\"character\":13},\"end\":{\"line\":4,\"character\":19}}}]}" ;;
    'textDocument/definition|src/main.ts|0,25' | 'textDocument/definition|src/main.ts|2,24' | 'textDocument/definition|src/main.ts|2,27' | 'textDocument/definition|src/main.ts|5,16' | 'textDocument/definition|src/main.ts|5,19' | 'textDocument/definition|src/shapes.ts|16,16')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/shapes.ts\",\"range\":{\"start\":{\"line\":16,\"character\":16},\"end\":{\"line\":16,\"character\":20}}}]}" ;;
    'textDocument/definition|src/main.ts|0,31' | 'textDocument/definition|src/main.ts|18,16' | 'textDocument/definition|src/main.ts|18,9' | 'textDocument/definition|src/shapes.ts|20,13')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/shapes.ts\",\"range\":{\"start\":{\"line\":20,\"character\":13},\"end\":{\"line\":20,\"character\":21}}}]}" ;;
    'textDocument/definition|src/main.ts|0,9' | 'textDocument/definition|src/main.ts|22,14' | 'textDocument/definition|src/main.ts|22,19' | 'textDocument/definition|src/shapes.ts|10,13' | 'textDocument/definition|src/shapes.ts|20,38' | 'textDocument/definition|src/shapes.ts|20,43' | 'textDocument/typeDefinition|src/shapes.ts|20,26')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/shapes.ts\",\"range\":{\"start\":{\"line\":10,\"character\":13},\"end\":{\"line\":10,\"character\":19}}}]}" ;;
    'textDocument/definition|src/main.ts|10,6' | 'textDocument/definition|src/main.ts|12,4' | 'textDocument/definition|src/main.ts|12,9' | 'textDocument/definition|src/main.ts|14,14' | 'textDocument/definition|src/main.ts|14,9')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/main.ts\",\"range\":{\"start\":{\"line\":10,\"character\":6},\"end\":{\"line\":10,\"character\":12}}}]}" ;;
    'textDocument/definition|src/main.ts|11,13' | 'textDocument/definition|src/main.ts|12,14' | 'textDocument/definition|src/main.ts|12,18')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/main.ts\",\"range\":{\"start\":{\"line\":11,\"character\":13},\"end\":{\"line\":11,\"character\":18}}}]}" ;;
    'textDocument/definition|src/main.ts|12,20' | 'textDocument/definition|src/main.ts|12,23' | 'textDocument/definition|src/main.ts|6,15' | 'textDocument/definition|src/main.ts|6,18' | 'textDocument/definition|src/shapes.ts|5,2' | 'textDocument/typeDefinition|src/shapes.ts|5,2')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/shapes.ts\",\"range\":{\"start\":{\"line\":5,\"character\":2},\"end\":{\"line\":5,\"character\":6}}}]}" ;;
    'textDocument/definition|src/main.ts|17,16' | 'textDocument/references|src/main.ts|17,16' | 'textDocument/typeDefinition|src/main.ts|17,16')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/main.ts\",\"range\":{\"start\":{\"line\":17,\"character\":16},\"end\":{\"line\":17,\"character\":22}}}]}" ;;
    'textDocument/definition|src/main.ts|18,18' | 'textDocument/definition|src/main.ts|18,23' | 'textDocument/definition|src/shapes.ts|20,26')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/shapes.ts\",\"range\":{\"start\":{\"line\":20,\"character\":26},\"end\":{\"line\":20,\"character\":32}}}]}" ;;
    'textDocument/definition|src/main.ts|18,25' | 'textDocument/definition|src/main.ts|18,28' | 'textDocument/definition|src/main.ts|22,45' | 'textDocument/definition|src/main.ts|22,48' | 'textDocument/definition|src/shapes.ts|11,2' | 'textDocument/typeDefinition|src/shapes.ts|11,2')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/shapes.ts\",\"range\":{\"start\":{\"line\":11,\"character\":2},\"end\":{\"line\":11,\"character\":6}}}]}" ;;
    'textDocument/definition|src/main.ts|2,13' | 'textDocument/references|src/main.ts|2,13')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/main.ts\",\"range\":{\"start\":{\"line\":2,\"character\":13},\"end\":{\"line\":2,\"character\":21}}}]}" ;;
    'textDocument/definition|src/main.ts|21,16' | 'textDocument/references|src/main.ts|21,16')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/main.ts\",\"range\":{\"start\":{\"line\":21,\"character\":16},\"end\":{\"line\":21,\"character\":22}}}]}" ;;
    'textDocument/definition|src/main.ts|22,26')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"file:///recorded/outside/lib/lib.es5.d.ts\",\"range\":{\"start\":{\"line\":1469,\"character\":4},\"end\":{\"line\":1469,\"character\":7}}}]}" ;;
    'textDocument/definition|src/main.ts|22,43')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/main.ts\",\"range\":{\"start\":{\"line\":22,\"character\":29},\"end\":{\"line\":22,\"character\":34}}}]}" ;;
    'textDocument/definition|src/main.ts|25,16' | 'textDocument/references|src/main.ts|25,16' | 'textDocument/typeDefinition|src/main.ts|25,16')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/main.ts\",\"range\":{\"start\":{\"line\":25,\"character\":16},\"end\":{\"line\":25,\"character\":21}}}]}" ;;
    'textDocument/definition|src/main.ts|26,14')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"file:///recorded/outside/lib/lib.es5.d.ts\",\"range\":{\"start\":{\"line\":409,\"character\":10},\"end\":{\"line\":409,\"character\":16}}},{\"uri\":\"file:///recorded/outside/lib/lib.es5.d.ts\",\"range\":{\"start\":{\"line\":543,\"character\":12},\"end\":{\"line\":543,\"character\":18}}},{\"uri\":\"file:///recorded/outside/lib/lib.es5.d.ts\",\"range\":{\"start\":{\"line\":4561,\"character\":10},\"end\":{\"line\":4561,\"character\":16}}},{\"uri\":\"file:///recorded/outside/lib/lib.es2015.core.d.ts\",\"range\":{\"start\":{\"line\":401,\"character\":10},\"end\":{\"line\":401,\"character\":16}}},{\"uri\":\"file:///recorded/outside/lib/lib.es2015.iterable.d.ts\",\"range\":{\"start\":{\"line\":269,\"character\":10},\"end\":{\"line\":269,\"character\":16}}},{\"uri\":\"file:///recorded/outside/lib/lib.es2015.symbol.wellknown.d.ts\",\"range\":{\"start\":{\"line\":234,\"character\":10},\"end\":{\"line\":234,\"character\":16}}},{\"uri\":\"file:///recorded/outside/lib/lib.es2017.string.d.ts\",\"range\":{\"start\":{\"line\":18,\"character\":10},\"end\":{\"line\":18,\"character\":16}}},{\"uri\":\"file:///recorded/outside/lib/lib.es2019.string.d.ts\",\"range\":{\"start\":{\"line\":18,\"character\":10},\"end\":{\"line\":18,\"character\":16}}},{\"uri\":\"file:///recorded/outside/lib/lib.es2020.string.d.ts\",\"range\":{\"start\":{\"line\":22,\"character\":10},\"end\":{\"line\":22,\"character\":16}}},{\"uri\":\"file:///recorded/outside/lib/lib.es2021.string.d.ts\",\"range\":{\"start\":{\"line\":18,\"character\":10},\"end\":{\"line\":18,\"character\":16}}},{\"uri\":\"file:///recorded/outside/lib/lib.es2022.string.d.ts\",\"range\":{\"start\":{\"line\":18,\"character\":10},\"end\":{\"line\":18,\"character\":16}}},{\"uri\":\"file:///recorded/outside/lib/lib.es5.d.ts\",\"range\":{\"start\":{\"line\":535,\"character\":4},\"end\":{\"line\":535,\"character\":26}}}]}" ;;
    'textDocument/definition|src/main.ts|26,25')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"file:///recorded/outside/lib/lib.es5.d.ts\",\"range\":{\"start\":{\"line\":516,\"character\":13},\"end\":{\"line\":516,\"character\":19}}}]}" ;;
    'textDocument/definition|src/main.ts|26,38')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"file:///recorded/outside/lib/lib.es5.d.ts\",\"range\":{\"start\":{\"line\":507,\"character\":4},\"end\":{\"line\":507,\"character\":15}}}]}" ;;
    'textDocument/definition|src/main.ts|4,16' | 'textDocument/references|src/main.ts|4,16' | 'textDocument/typeDefinition|src/main.ts|4,16')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/main.ts\",\"range\":{\"start\":{\"line\":4,\"character\":16},\"end\":{\"line\":4,\"character\":21}}}]}" ;;
    'textDocument/definition|src/main.ts|5,8' | 'textDocument/definition|src/main.ts|6,13' | 'textDocument/definition|src/main.ts|6,9')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/main.ts\",\"range\":{\"start\":{\"line\":5,\"character\":8},\"end\":{\"line\":5,\"character\":13}}}]}" ;;
    'textDocument/definition|src/main.ts|9,16' | 'textDocument/references|src/main.ts|9,16' | 'textDocument/typeDefinition|src/main.ts|9,16')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/main.ts\",\"range\":{\"start\":{\"line\":9,\"character\":16},\"end\":{\"line\":9,\"character\":20}}}]}" ;;
    'textDocument/definition|src/shapes.ts|0,17' | 'textDocument/definition|src/shapes.ts|10,31' | 'textDocument/definition|src/shapes.ts|10,35' | 'textDocument/definition|src/shapes.ts|4,31' | 'textDocument/definition|src/shapes.ts|4,35' | 'textDocument/typeDefinition|src/shapes.ts|0,17')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/shapes.ts\",\"range\":{\"start\":{\"line\":0,\"character\":17},\"end\":{\"line\":0,\"character\":22}}}]}" ;;
    'textDocument/definition|src/shapes.ts|1,2' | 'textDocument/typeDefinition|src/shapes.ts|1,2')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/shapes.ts\",\"range\":{\"start\":{\"line\":1,\"character\":2},\"end\":{\"line\":1,\"character\":6}}}]}" ;;
    'textDocument/implementation|src/shapes.ts|0,17')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/shapes.ts\",\"range\":{\"start\":{\"line\":4,\"character\":13},\"end\":{\"line\":4,\"character\":19}}},{\"uri\":\"$root/src/shapes.ts\",\"range\":{\"start\":{\"line\":10,\"character\":13},\"end\":{\"line\":10,\"character\":19}}}]}" ;;
    'textDocument/prepareCallHierarchy|src/main.ts|10,6' | 'textDocument/prepareCallHierarchy|src/main.ts|11,13' | 'textDocument/prepareCallHierarchy|src/main.ts|2,13' | 'textDocument/prepareCallHierarchy|src/main.ts|5,8' | 'textDocument/prepareCallHierarchy|src/shapes.ts|0,17' | 'textDocument/prepareCallHierarchy|src/shapes.ts|20,13' | 'textDocument/prepareCallHierarchy|src/shapes.ts|20,26')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":null}" ;;
    'textDocument/prepareCallHierarchy|src/main.ts|17,16')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"kind\":12,\"name\":\"lookup\",\"detail\":\"\",\"uri\":\"$root/src/main.ts\",\"range\":{\"start\":{\"line\":17,\"character\":0},\"end\":{\"line\":19,\"character\":1}},\"selectionRange\":{\"start\":{\"line\":17,\"character\":16},\"end\":{\"line\":17,\"character\":22}}}]}" ;;
    'textDocument/prepareCallHierarchy|src/main.ts|21,16')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"kind\":12,\"name\":\"mapped\",\"detail\":\"\",\"uri\":\"$root/src/main.ts\",\"range\":{\"start\":{\"line\":21,\"character\":0},\"end\":{\"line\":23,\"character\":1}},\"selectionRange\":{\"start\":{\"line\":21,\"character\":16},\"end\":{\"line\":21,\"character\":22}}}]}" ;;
    'textDocument/prepareCallHierarchy|src/main.ts|25,16')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"kind\":12,\"name\":\"label\",\"detail\":\"\",\"uri\":\"$root/src/main.ts\",\"range\":{\"start\":{\"line\":25,\"character\":0},\"end\":{\"line\":27,\"character\":1}},\"selectionRange\":{\"start\":{\"line\":25,\"character\":16},\"end\":{\"line\":25,\"character\":21}}}]}" ;;
    'textDocument/prepareCallHierarchy|src/main.ts|4,16')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"kind\":12,\"name\":\"total\",\"detail\":\"\",\"uri\":\"$root/src/main.ts\",\"range\":{\"start\":{\"line\":4,\"character\":0},\"end\":{\"line\":7,\"character\":1}},\"selectionRange\":{\"start\":{\"line\":4,\"character\":16},\"end\":{\"line\":4,\"character\":21}}}]}" ;;
    'textDocument/prepareCallHierarchy|src/main.ts|9,16')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"kind\":12,\"name\":\"each\",\"detail\":\"\",\"uri\":\"$root/src/main.ts\",\"range\":{\"start\":{\"line\":9,\"character\":0},\"end\":{\"line\":15,\"character\":1}},\"selectionRange\":{\"start\":{\"line\":9,\"character\":16},\"end\":{\"line\":9,\"character\":20}}}]}" ;;
    'textDocument/prepareCallHierarchy|src/shapes.ts|1,2')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"kind\":6,\"name\":\"area\",\"detail\":\"\",\"uri\":\"$root/src/shapes.ts\",\"range\":{\"start\":{\"line\":1,\"character\":2},\"end\":{\"line\":1,\"character\":17}},\"selectionRange\":{\"start\":{\"line\":1,\"character\":2},\"end\":{\"line\":1,\"character\":6}}}]}" ;;
    'textDocument/prepareCallHierarchy|src/shapes.ts|10,13')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"kind\":5,\"name\":\"Circle\",\"detail\":\"\",\"uri\":\"$root/src/shapes.ts\",\"range\":{\"start\":{\"line\":10,\"character\":0},\"end\":{\"line\":14,\"character\":1}},\"selectionRange\":{\"start\":{\"line\":10,\"character\":13},\"end\":{\"line\":10,\"character\":19}}}]}" ;;
    'textDocument/prepareCallHierarchy|src/shapes.ts|11,2')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"kind\":6,\"name\":\"area\",\"detail\":\"Circle\",\"uri\":\"$root/src/shapes.ts\",\"range\":{\"start\":{\"line\":11,\"character\":2},\"end\":{\"line\":13,\"character\":3}},\"selectionRange\":{\"start\":{\"line\":11,\"character\":2},\"end\":{\"line\":11,\"character\":6}}}]}" ;;
    'textDocument/prepareCallHierarchy|src/shapes.ts|16,16')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"kind\":12,\"name\":\"make\",\"detail\":\"\",\"uri\":\"$root/src/shapes.ts\",\"range\":{\"start\":{\"line\":16,\"character\":0},\"end\":{\"line\":18,\"character\":1}},\"selectionRange\":{\"start\":{\"line\":16,\"character\":16},\"end\":{\"line\":16,\"character\":20}}}]}" ;;
    'textDocument/prepareCallHierarchy|src/shapes.ts|4,13')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"kind\":5,\"name\":\"Square\",\"detail\":\"\",\"uri\":\"$root/src/shapes.ts\",\"range\":{\"start\":{\"line\":4,\"character\":0},\"end\":{\"line\":8,\"character\":1}},\"selectionRange\":{\"start\":{\"line\":4,\"character\":13},\"end\":{\"line\":4,\"character\":19}}}]}" ;;
    'textDocument/prepareCallHierarchy|src/shapes.ts|5,2')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"kind\":6,\"name\":\"area\",\"detail\":\"Square\",\"uri\":\"$root/src/shapes.ts\",\"range\":{\"start\":{\"line\":5,\"character\":2},\"end\":{\"line\":7,\"character\":3}},\"selectionRange\":{\"start\":{\"line\":5,\"character\":2},\"end\":{\"line\":5,\"character\":6}}}]}" ;;
    'textDocument/references|src/main.ts|10,6')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/main.ts\",\"range\":{\"start\":{\"line\":10,\"character\":6},\"end\":{\"line\":10,\"character\":12}}},{\"uri\":\"$root/src/main.ts\",\"range\":{\"start\":{\"line\":12,\"character\":4},\"end\":{\"line\":12,\"character\":10}}},{\"uri\":\"$root/src/main.ts\",\"range\":{\"start\":{\"line\":14,\"character\":9},\"end\":{\"line\":14,\"character\":15}}}]}" ;;
    'textDocument/references|src/main.ts|11,13')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/main.ts\",\"range\":{\"start\":{\"line\":11,\"character\":13},\"end\":{\"line\":11,\"character\":18}}},{\"uri\":\"$root/src/main.ts\",\"range\":{\"start\":{\"line\":12,\"character\":14},\"end\":{\"line\":12,\"character\":19}}}]}" ;;
    'textDocument/references|src/main.ts|5,8')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/main.ts\",\"range\":{\"start\":{\"line\":5,\"character\":8},\"end\":{\"line\":5,\"character\":13}}},{\"uri\":\"$root/src/main.ts\",\"range\":{\"start\":{\"line\":6,\"character\":9},\"end\":{\"line\":6,\"character\":14}}}]}" ;;
    'textDocument/references|src/shapes.ts|0,17')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/shapes.ts\",\"range\":{\"start\":{\"line\":0,\"character\":17},\"end\":{\"line\":0,\"character\":22}}},{\"uri\":\"$root/src/shapes.ts\",\"range\":{\"start\":{\"line\":4,\"character\":31},\"end\":{\"line\":4,\"character\":36}}},{\"uri\":\"$root/src/shapes.ts\",\"range\":{\"start\":{\"line\":10,\"character\":31},\"end\":{\"line\":10,\"character\":36}}}]}" ;;
    'textDocument/references|src/shapes.ts|1,2' | 'textDocument/references|src/shapes.ts|11,2' | 'textDocument/references|src/shapes.ts|5,2')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/shapes.ts\",\"range\":{\"start\":{\"line\":1,\"character\":2},\"end\":{\"line\":1,\"character\":6}}},{\"uri\":\"$root/src/shapes.ts\",\"range\":{\"start\":{\"line\":5,\"character\":2},\"end\":{\"line\":5,\"character\":6}}},{\"uri\":\"$root/src/main.ts\",\"range\":{\"start\":{\"line\":6,\"character\":15},\"end\":{\"line\":6,\"character\":19}}},{\"uri\":\"$root/src/main.ts\",\"range\":{\"start\":{\"line\":12,\"character\":20},\"end\":{\"line\":12,\"character\":24}}},{\"uri\":\"$root/src/shapes.ts\",\"range\":{\"start\":{\"line\":11,\"character\":2},\"end\":{\"line\":11,\"character\":6}}},{\"uri\":\"$root/src/main.ts\",\"range\":{\"start\":{\"line\":18,\"character\":25},\"end\":{\"line\":18,\"character\":29}}},{\"uri\":\"$root/src/main.ts\",\"range\":{\"start\":{\"line\":22,\"character\":45},\"end\":{\"line\":22,\"character\":49}}}]}" ;;
    'textDocument/references|src/shapes.ts|10,13')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/shapes.ts\",\"range\":{\"start\":{\"line\":10,\"character\":13},\"end\":{\"line\":10,\"character\":19}}},{\"uri\":\"$root/src/shapes.ts\",\"range\":{\"start\":{\"line\":20,\"character\":38},\"end\":{\"line\":20,\"character\":44}}},{\"uri\":\"$root/src/main.ts\",\"range\":{\"start\":{\"line\":0,\"character\":9},\"end\":{\"line\":0,\"character\":15}}},{\"uri\":\"$root/src/main.ts\",\"range\":{\"start\":{\"line\":22,\"character\":14},\"end\":{\"line\":22,\"character\":20}}}]}" ;;
    'textDocument/references|src/shapes.ts|16,16')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/shapes.ts\",\"range\":{\"start\":{\"line\":16,\"character\":16},\"end\":{\"line\":16,\"character\":20}}},{\"uri\":\"$root/src/main.ts\",\"range\":{\"start\":{\"line\":0,\"character\":25},\"end\":{\"line\":0,\"character\":29}}},{\"uri\":\"$root/src/main.ts\",\"range\":{\"start\":{\"line\":2,\"character\":24},\"end\":{\"line\":2,\"character\":28}}},{\"uri\":\"$root/src/main.ts\",\"range\":{\"start\":{\"line\":5,\"character\":16},\"end\":{\"line\":5,\"character\":20}}}]}" ;;
    'textDocument/references|src/shapes.ts|20,13')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/shapes.ts\",\"range\":{\"start\":{\"line\":20,\"character\":13},\"end\":{\"line\":20,\"character\":21}}},{\"uri\":\"$root/src/main.ts\",\"range\":{\"start\":{\"line\":0,\"character\":31},\"end\":{\"line\":0,\"character\":39}}},{\"uri\":\"$root/src/main.ts\",\"range\":{\"start\":{\"line\":18,\"character\":9},\"end\":{\"line\":18,\"character\":17}}}]}" ;;
    'textDocument/references|src/shapes.ts|20,26')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/shapes.ts\",\"range\":{\"start\":{\"line\":20,\"character\":26},\"end\":{\"line\":20,\"character\":32}}},{\"uri\":\"$root/src/main.ts\",\"range\":{\"start\":{\"line\":18,\"character\":18},\"end\":{\"line\":18,\"character\":24}}}]}" ;;
    'textDocument/references|src/shapes.ts|4,13')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/shapes.ts\",\"range\":{\"start\":{\"line\":4,\"character\":13},\"end\":{\"line\":4,\"character\":19}}},{\"uri\":\"$root/src/shapes.ts\",\"range\":{\"start\":{\"line\":17,\"character\":13},\"end\":{\"line\":17,\"character\":19}}},{\"uri\":\"$root/src/main.ts\",\"range\":{\"start\":{\"line\":0,\"character\":17},\"end\":{\"line\":0,\"character\":23}}},{\"uri\":\"$root/src/main.ts\",\"range\":{\"start\":{\"line\":11,\"character\":27},\"end\":{\"line\":11,\"character\":33}}},{\"uri\":\"$root/src/main.ts\",\"range\":{\"start\":{\"line\":11,\"character\":41},\"end\":{\"line\":11,\"character\":47}}}]}" ;;
    'textDocument/typeDefinition|src/main.ts|21,16')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"file:///recorded/outside/lib/lib.es5.d.ts\",\"range\":{\"start\":{\"line\":1324,\"character\":10},\"end\":{\"line\":1324,\"character\":15}}},{\"uri\":\"file:///recorded/outside/lib/lib.es5.d.ts\",\"range\":{\"start\":{\"line\":1523,\"character\":12},\"end\":{\"line\":1523,\"character\":17}}},{\"uri\":\"file:///recorded/outside/lib/lib.es2015.core.d.ts\",\"range\":{\"start\":{\"line\":18,\"character\":10},\"end\":{\"line\":18,\"character\":15}}},{\"uri\":\"file:///recorded/outside/lib/lib.es2015.iterable.d.ts\",\"range\":{\"start\":{\"line\":75,\"character\":10},\"end\":{\"line\":75,\"character\":15}}},{\"uri\":\"file:///recorded/outside/lib/lib.es2015.symbol.wellknown.d.ts\",\"range\":{\"start\":{\"line\":91,\"character\":10},\"end\":{\"line\":91,\"character\":15}}},{\"uri\":\"file:///recorded/outside/lib/lib.es2016.array.include.d.ts\",\"range\":{\"start\":{\"line\":18,\"character\":10},\"end\":{\"line\":18,\"character\":15}}},{\"uri\":\"file:///recorded/outside/lib/lib.es2019.array.d.ts\",\"range\":{\"start\":{\"line\":52,\"character\":10},\"end\":{\"line\":52,\"character\":15}}},{\"uri\":\"file:///recorded/outside/lib/lib.es2022.array.d.ts\",\"range\":{\"start\":{\"line\":18,\"character\":10},\"end\":{\"line\":18,\"character\":15}}}]}" ;;
    'textDocument/typeDefinition|src/shapes.ts|20,13')
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$root/src/shapes.ts\",\"range\":{\"start\":{\"line\":20,\"character\":24},\"end\":{\"line\":20,\"character\":48}}}]}" ;;
    *) frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":null}" ;;
  esac
done
