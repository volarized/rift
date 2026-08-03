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

  env: { NEXT_PUBLIC_BASE_PATH: basePath },
};

export default withMDX(config);
