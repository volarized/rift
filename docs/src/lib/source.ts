import { loader } from "fumadocs-core/source";
import { docs } from "@/.source/server";
import { getDocsIcon } from "@/lib/docs-icons";

// See https://fumadocs.vercel.app/docs/headless/source-api for more info
export const source = loader({
  // it assigns a URL to your pages
  baseUrl: "/docs",
  source: docs.toFumadocsSource(),
  icon: getDocsIcon,
});
