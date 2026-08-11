import { createFromSource } from "fumadocs-core/search/server";
import { allPageData } from "@/lib/protocol-surface";
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

  // fumadocs derives `structuredData` from the MDX abstract syntax tree. A
  // protocol page is prose followed by a component that renders the schemas,
  // so remark sees the prose and nothing else — every tool, operation, and
  // type would be unsearchable. The derived half is appended here.
  buildIndex(page) {
    const derived = allPageData[page.url];
    const structured = page.data.structuredData;

    return {
      title: page.data.title,
      description: page.data.description,
      url: page.url,
      id: page.url,
      structuredData: derived
        ? {
            headings: [...structured.headings, ...derived.structuredData.headings],
            contents: [...structured.contents, ...derived.structuredData.contents],
          }
        : structured,
    };
  },
});
