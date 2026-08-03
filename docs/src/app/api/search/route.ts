import { createFromSource } from "fumadocs-core/search/server";
import { source } from "@/lib/source";

// `staticGET`, not `GET`: `output: "export"` cannot emit a dynamic Route
// Handler. This pre-renders the Orama index to a file instead, and search
// runs in the browser.
//
// Search is currently disabled in RootProvider (src/app/layout.tsx). If it is
// ever enabled, the client needs an explicit `from` — it defaults to
// "/api/search" and does NOT pick up next.config basePath:
//
//   oramaStaticClient({ from: "/rift/api/search" })
export const revalidate = false;
export const { staticGET: GET } = createFromSource(source, {
  // https://docs.orama.com/docs/orama-js/supported-languages
  language: "english",
});
