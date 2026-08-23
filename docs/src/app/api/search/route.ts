import { createFromSource } from "fumadocs-core/search/server";
import { source } from "@/lib/source";

// `staticGET`, not `GET`: `output: "export"` cannot emit a dynamic Route
// Handler. This pre-renders the Orama index to a file instead, and search
// runs in the browser.
//
// The client side is wired in RootProvider (src/app/layout.tsx) with
// `type: "static"` and an explicit `api` - the default is a bare
// "/api/search", which does NOT pick up next.config basePath and so 404s
// under /rift.
//
// `language` here must match the client's Orama instance, which fumadocs
// creates with the locale (undefined -> english). Changing one without the
// other yields an index that loads but stems differently than it was built.
export const revalidate = false;
export const { staticGET: GET } = createFromSource(source, {
  // https://docs.orama.com/docs/orama-js/supported-languages
  language: "english",
});
