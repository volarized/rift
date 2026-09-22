import { createMDX } from "fumadocs-mdx/next";
import { basePath } from "./scripts/base-path.mjs";

const withMDX = createMDX();

/** @type {import('next').NextConfig} */
const config = {
  reactStrictMode: true,

  // Served as static assets by an assets-only Cloudflare Worker at
  // volar.sh/rift — no Node runtime, so no SSR, ISR, or dynamic routes.
  output: "export",
  basePath,
  trailingSlash: true,

  // next/image optimisation needs a server; static export has none.
  images: { unoptimized: true },

  // `mermaid-parser-bundle` is jison-generated, and Turbopack's minifier emits
  // a duplicate declaration when it mangles that output, which fails the build
  // with `Identifier 't' has already been declared`. The module is imported by
  // a React Server Component and runs under Node during the build, so leaving
  // it external keeps it a plain runtime import that no minifier rewrites.
  serverExternalPackages: ["mermaid-parser-bundle"],

  env: { NEXT_PUBLIC_BASE_PATH: basePath },
};

export default withMDX(config);
