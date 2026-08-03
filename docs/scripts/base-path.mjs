/**
 * Where the docs are served from: https://volar.sh/rift
 *
 * Single source of truth, imported by `../next.config.mjs` (to set `basePath`
 * and export `NEXT_PUBLIC_BASE_PATH`) and by `./pack.mjs` (to verify the built
 * output actually uses it). Kept in its own module so the pack script can read
 * it without importing the Next config and triggering the MDX plugin's side
 * effects.
 */
export const basePath = "/rift";
