/** Default endpoint of the global API, as documented under Configuration - Global. */
export const GLOBAL_REST_ENDPOINT = "https://api.volar.sh/rift/rest";

/** Default endpoint of the MCP server of the global index, over Streamable HTTP. */
export const GLOBAL_MCP_ENDPOINT = "https://api.volar.sh/rift/mcp";

/** Header a caller sends to select the API version, and the value this site sends. */
export const RIFT_API_VERSION_HEADER = "Rift-Api-Version";
export const RIFT_API_VERSION = "2";

// Next applies `basePath` to `next/link` and the router only, so a raw asset
// URL has to carry it by hand or it 404s under /rift.
const BASE_PATH = process.env.NEXT_PUBLIC_BASE_PATH ?? "";

/** URL of the OpenAPI document of the global API, served from this site's own origin. */
export function globalOpenApiUrl(): string {
  return `${BASE_PATH}/global-api.openapi.json`;
}
