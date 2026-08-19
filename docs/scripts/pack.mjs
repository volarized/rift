#!/usr/bin/env node
/**
 * Packs the Next static export into the layout the Cloudflare Worker serves,
 * then verifies it before it can reach production.
 *
 *   out/**         ->  dist/rift/**    the site; every URL prefixed /rift
 *
 * Next emits the export at the *root* of out/ even though basePath is /rift,
 * so the tree has to be nested one level by hand.
 *
 * Usage: bun run pack
 */

import { mkdir, readdir, readFile, rename, rm } from "node:fs/promises";
import path from "node:path";
import { argv, exit } from "node:process";

import { basePath } from "./base-path.mjs";

const docsRoot = path.resolve(import.meta.dirname, "..");
const outDir = path.join(docsRoot, "out");
const distDir = path.join(docsRoot, "dist");
const siteDir = path.join(distDir, basePath.replace(/^\//, ""));

/** Files that must exist once packing is done, relative to dist/. */
const required = [
  "_headers",
  path.join(basePath.replace(/^\//, ""), "index.html"),
  path.join(basePath.replace(/^\//, ""), "docs", "index.html"),
  path.join(basePath.replace(/^\//, ""), "install.sh"),
  path.join(basePath.replace(/^\//, ""), "install.ps1"),
  path.join(basePath.replace(/^\//, ""), "404.html"),
];

/** Recursively yield every file under `dir` matching `predicate`. */
async function* walk(dir, predicate) {
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) yield* walk(full, predicate);
    else if (predicate(full)) yield full;
  }
}

async function pack() {
  try {
    await readdir(outDir);
  } catch {
    console.error("✗ no out/ directory — run `bun run build` first");
    exit(1);
  }

  await rm(distDir, { recursive: true, force: true });
  await mkdir(distDir, { recursive: true });
  // `rename` rather than copy: the export is single-use build output, and
  // moving it keeps `out/` from lingering as a stale second copy.
  await rename(outDir, siteDir);
  // Workers Static Assets reads control files only from the asset root.
  await rename(path.join(siteDir, "_headers"), path.join(distDir, "_headers"));
}

/**
 * Absolute URLs that do not start with the basePath resolve against the apex
 * and 404 in production. Next rewrites routes and /_next assets itself, but
 * not everything — `metadata.icons` is the known offender.
 */
async function findUnprefixedRefs() {
  const pattern = /(?:href|src)="(\/[^"]*)"/g;
  const offenders = [];

  for await (const file of walk(siteDir, (f) => f.endsWith(".html"))) {
    const html = await readFile(file, "utf8");
    for (const [, url] of html.matchAll(pattern)) {
      if (url === basePath || url.startsWith(`${basePath}/`)) continue;
      offenders.push({ file: path.relative(distDir, file), url });
    }
  }
  return offenders;
}

async function verify() {
  const problems = [];

  for (const rel of required) {
    try {
      await readFile(path.join(distDir, rel));
    } catch {
      problems.push(`missing required file: dist/${rel}`);
    }
  }

  for (const { file, url } of await findUnprefixedRefs()) {
    problems.push(`${file}: "${url}" is missing the ${basePath} prefix`);
  }

  const headers = await readFile(path.join(distDir, "_headers"), "utf8");
  for (const installer of ["install.sh", "install.ps1"]) {
    const rule = `${basePath}/${installer}\n  Content-Type: text/plain; charset=utf-8`;
    if (!headers.includes(rule)) {
      problems.push(`_headers: missing text/plain rule for ${basePath}/${installer}`);
    }
  }

  return problems;
}

const verifyOnly = argv.includes("--verify-only");
if (!verifyOnly) await pack();

const problems = await verify();
if (problems.length > 0) {
  for (const problem of problems) console.error(`✗ ${problem}`);
  // GitHub Actions surfaces this as an annotation on the failed step.
  console.error(`::error::pack verification failed (${problems.length} problems)`);
  exit(1);
}

console.log(`✓ dist/ packed and verified — site at dist${basePath}/`);
