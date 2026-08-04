import { createFromSource } from "fumadocs-core/search/server";
import { PROTOCOL_PAGE_URL, protocolStructuredData } from "@/lib/protocol";
import { source } from "@/lib/source";

// `staticGET`, not `GET`: `output: "export"` cannot emit a dynamic Route
// Handler. This pre-renders the Orama index to a file instead, and search
// runs in the browser.
//
// The client side is wired in RootProvider (src/app/layout.tsx) with
// `type: "static"` and an explicit `api` — the default is a bare
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

  // fumadocs derives `structuredData` from the MDX abstract syntax tree, and
  // the protocol page's body is a component rather than prose — so remark sees
  // one JSX element and the page would be unsearchable. Its index is built from
  // protocol/definition.json instead, headings and descriptions only.
  buildIndex(page) {
    return {
      title: page.data.title,
      description: page.data.description,
      url: page.url,
      id: page.url,
      structuredData:
        page.url === PROTOCOL_PAGE_URL ? protocolStructuredData : page.data.structuredData,
    };
  },
});
