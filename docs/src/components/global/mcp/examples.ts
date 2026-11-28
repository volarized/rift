import type { Arguments, FieldSpec } from "@/components/global/mcp/schema-form";

/** One named call the console offers for a tool. */
export type ToolExample = { label: string; arguments: Arguments };

/**
 * An older connected server can still advertise boolean include fields.
 * Convert the example's selection to that server's advertised shape.
 */
export function argumentsFor(example: ToolExample, specs: FieldSpec[]): Arguments {
  const include = example.arguments.include;
  if (
    !Array.isArray(include) ||
    !specs.some((spec) => spec.name === "include" && spec.typeLabel === "object")
  ) {
    return example.arguments;
  }
  return {
    ...example.arguments,
    include: Object.fromEntries(
      include.filter((member) => typeof member === "string").map((member) => [member, true]),
    ),
  };
}

type Package = { manager: string; registry: string; name: string; version?: string };

const ITOA: Package = { manager: "cargo", registry: "crates.io", name: "itoa", version: "1.0.18" };
const ITOA_REVISION = "rift://revision/cargo/crates.io/itoa@1.0.18";
const ITOA_BUFFER = "rift://symbol/cargo/crates.io/itoa/rust/itoa/Buffer";

/**
 * A ranked search of one package's symbols. The global index answers a search
 * for the packages it names: each entry carries its `registry`, and one without
 * a `version` reads the newest collected release.
 */
function symbolSearch(text: string, target: Package, projection = "?symbol ?name"): string {
  const source = projection.includes("?source") ? " :source ?source;" : "";
  return `SELECT ${projection} {
  SERVICE :search {
    ?hit :query "${text}"; :target "symbol"; :scope "global";
         :packages '${JSON.stringify([target])}'^^rdf:JSON;
         :symbol ?symbol;${source} :index ?index .
    ?symbol :name ?name
  }
}
ORDER BY ?index
LIMIT 5`;
}

const PATTERN_SEARCH = `SELECT ?symbol ?source {
  SERVICE :search {
    ?hit :pattern "pub fn format"; :target "symbol"; :scope "global";
         :packages '${JSON.stringify([ITOA])}'^^rdf:JSON;
         :symbol ?symbol; :source ?source
  }
}
LIMIT 5`;

/**
 * Calls the global endpoint answers with data, by tool name. The schemas of a
 * Rift server carry examples that read a local project, which the global
 * server has none of, so the console offers these in their place. A tool
 * missing here keeps the examples of its own schema.
 */
export const GLOBAL_EXAMPLES: Record<string, ToolExample[]> = {
  search: [
    {
      label: "Symbols by name",
      arguments: {
        query: symbolSearch("Serialize", {
          manager: "cargo",
          registry: "crates.io",
          name: "serde_core",
        }),
      },
    },
    {
      label: "Symbols with source",
      arguments: { query: symbolSearch("Buffer", ITOA, "?symbol ?name ?source") },
    },
    { label: "Regex match", arguments: { query: PATTERN_SEARCH } },
    {
      label: "npm package",
      arguments: {
        query: symbolSearch("useState", { manager: "npm", registry: "npmjs.org", name: "react" }),
      },
    },
    {
      label: "PyPI package",
      arguments: {
        query: symbolSearch("Session", {
          manager: "pypi",
          registry: "pypi.org",
          name: "requests",
        }),
      },
    },
  ],
  get_symbol: [
    {
      label: "Rust struct",
      arguments: {
        id: ITOA_BUFFER,
        revision: ITOA_REVISION,
        include: ["documentation"],
      },
    },
    {
      label: "Python class",
      arguments: {
        id: "rift://symbol/pypi/pypi.org/requests/python/requests/sessions/Session",
        include: ["documentation"],
      },
    },
    {
      label: "Node runtime",
      arguments: { id: "rift://symbol/stdlib/node/javascript/fs/readFile" },
    },
  ],
  facets: [
    {
      label: "Rust struct",
      arguments: { symbol: ITOA_BUFFER, revision: ITOA_REVISION },
    },
    {
      label: "Rust method",
      arguments: { symbol: `${ITOA_BUFFER}/new`, revision: ITOA_REVISION },
    },
    {
      label: "Python method",
      arguments: {
        symbol: "rift://symbol/pypi/pypi.org/requests/python/requests/sessions/Session/get",
      },
    },
  ],
  // A server from before `facets` lists `nodes` in its place.
  nodes: [
    {
      label: "Declaration source",
      arguments: { symbol: ITOA_BUFFER, revision: ITOA_REVISION },
    },
    {
      label: "Python method",
      arguments: {
        symbol: "rift://symbol/pypi/pypi.org/requests/python/requests/sessions/Session/get",
      },
    },
    {
      label: "Syntax tree",
      arguments: { symbol: `${ITOA_BUFFER}/new`, revision: ITOA_REVISION, syntax: true },
    },
  ],
};
