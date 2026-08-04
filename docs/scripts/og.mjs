/**
 * Generates public/og.png — the 1280x640 social card used as the Open Graph
 * image for the docs and as the repository's social preview on GitHub.
 *
 *   node scripts/og.mjs           # -> public/og.png
 *   node scripts/og.mjs out.png   # -> arbitrary path
 *
 * The card is laid out in HTML and photographed by a headless Chrome, because
 * the two things that make it look like the site — Geist Mono and Inter, and a
 * hairline mesh that has to survive downsampling — are a browser's job. It is
 * shot at 2x and reduced to 1280x640 by sharp, so the strings stay crisp.
 *
 * Needs a Chrome/Chromium on the machine (set CHROME to override the search)
 * and network, to pull the two webfonts from Google Fonts and inline them.
 */
import { spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import sharp from "sharp";

const HERE = dirname(fileURLToPath(import.meta.url));

const OUT = resolve(HERE, "..", process.argv[2] ?? "public/og.png");

/** GitHub asks for 1280x640 for the social preview; OG scrapers are happy with it. */
const WIDTH = 1280;
const HEIGHT = 640;

/** Shot at this multiple and reduced back down, to keep the hairlines from aliasing. */
const SUPERSAMPLE = 2;

const TITLE = "rift";
const TAGLINE = ["Agentic development toolkit for reading,", "discovering and editing codebases."];

/** The four values of the design system. */
const VOID = "#08080A";
const INK = "#C9C9C4";

/** Strings per side of the mark, matching the <Logo> default. */
const STITCHES = 26;

const CHROMES = [
  process.env.CHROME,
  "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  "/Applications/Chromium.app/Contents/MacOS/Chromium",
  "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
  "/usr/bin/google-chrome",
  "/usr/bin/chromium",
  "/usr/bin/chromium-browser",
];

function findChrome() {
  const found = CHROMES.find((path) => path && existsSync(path));
  if (!found) {
    throw new Error(
      `No Chrome found. Tried:\n  ${CHROMES.filter(Boolean).join("\n  ")}\nSet CHROME=/path/to/chrome.`,
    );
  }
  return found;
}

/**
 * The mark, as a standalone <svg> string. Same construction as
 * src/components/logo.tsx: an inverted equilateral triangle whose sides are
 * subdivided, every division point joined to its mirror on the next side, so
 * the straight strings envelope a parabola.
 */
function mark({ stitches = STITCHES, strokeWidth = 0.25, stringOpacity = 0.62 } = {}) {
  const VIEW_W = 100;
  const PAD = 2;
  const edgeWidth = strokeWidth * 1.7;

  const vertices = [0, 1, 2].map((k) => {
    const theta = -Math.PI / 2 + (k / 3) * Math.PI * 2;
    return { x: Math.cos(theta), y: Math.sin(theta) };
  });

  const yTop = Math.max(...vertices.map((v) => v.y));
  const scale = VIEW_W / 2 / Math.max(...vertices.map((v) => Math.abs(v.x)));
  const viewH = (yTop - Math.min(...vertices.map((v) => v.y))) * scale;

  const place = (p) => ({ x: VIEW_W / 2 + p.x * scale, y: (yTop - p.y) * scale });
  const lerp = (a, b, t) => ({ x: a.x + (b.x - a.x) * t, y: a.y + (b.y - a.y) * t });
  const round = (n) => Number(n.toFixed(3));

  const line = (from, to, width, opacity) =>
    `<line x1="${round(from.x)}" y1="${round(from.y)}" x2="${round(to.x)}" y2="${round(to.y)}" ` +
    `stroke-width="${width}" opacity="${opacity}"/>`;

  const lines = [];
  for (let corner = 0; corner < 3; corner++) {
    const a = vertices[corner];
    const b = vertices[(corner + 1) % 3];
    const d = vertices[(corner + 2) % 3];

    lines.push(line(place(a), place(b), edgeWidth, 1));

    for (let i = 1; i < stitches; i++) {
      const u = i / stitches;
      lines.push(line(place(lerp(a, b, u)), place(lerp(a, d, 1 - u)), strokeWidth, stringOpacity));
    }
  }

  return (
    `<svg xmlns="http://www.w3.org/2000/svg" ` +
    `viewBox="${-PAD} ${-PAD} ${VIEW_W + PAD * 2} ${round(viewH + PAD * 2)}" ` +
    `role="img" aria-label="rift">` +
    `<g fill="none" stroke="${INK}" stroke-linecap="round">${lines.join("")}</g>` +
    `</svg>`
  );
}

/** Pretending to be a browser, or Google Fonts serves TTF instead of woff2. */
const UA =
  "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 " +
  "(KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/**
 * The latin cut of one Google font, inlined as a data: URI — the card has to
 * render from a file:// URL with no network fetches of its own.
 */
async function webfont(family, weight) {
  const url =
    `https://fonts.googleapis.com/css2?family=${encodeURIComponent(family)}` +
    `:wght@${weight}&display=block`;

  const response = await fetch(url, { headers: { "user-agent": UA } });
  if (!response.ok) throw new Error(`${family} ${weight}: CSS ${response.status}`);
  const css = await response.text();

  // Every subset arrives as its own @font-face, labelled by a comment; the
  // card is latin-only, so take that one and leave the other dozen behind.
  const blocks = [...css.matchAll(/\/\*\s*([\w-]+)\s*\*\/\s*@font-face\s*\{([^}]*)\}/g)];
  const latin = blocks.find(([, subset]) => subset === "latin");
  if (!latin) throw new Error(`${family} ${weight}: no latin subset in ${url}`);

  const src = latin[2].match(/url\((https:[^)]+\.woff2)\)/);
  if (!src) throw new Error(`${family} ${weight}: no woff2 in the latin subset`);

  const font = await fetch(src[1], { headers: { "user-agent": UA } });
  if (!font.ok) throw new Error(`${family} ${weight}: font ${font.status}`);
  const base64 = Buffer.from(await font.arrayBuffer()).toString("base64");

  return (
    `@font-face{font-family:"${family}";font-style:normal;font-weight:${weight};` +
    `src:url(data:font/woff2;base64,${base64}) format("woff2")}`
  );
}

function card(fonts) {
  return `<!doctype html>
<meta charset="utf-8">
<style>
  ${fonts.join("\n  ")}

  * { margin: 0; padding: 0; box-sizing: border-box; }

  body {
    width: ${WIDTH}px;
    height: ${HEIGHT}px;
    background: ${VOID};
    color: ${INK};
    display: flex;
    align-items: center;
    justify-content: space-between;
    /* Well outside GitHub's 40pt safe area, so nothing is cropped away. */
    padding: 0 84px;
    overflow: hidden;
    -webkit-font-smoothing: antialiased;
  }

  h1 {
    font: 400 122px/1 "Geist Mono", monospace;
    letter-spacing: -0.01em;
  }

  p {
    margin-top: 46px;
    font: 300 27px/1.5 "Inter", sans-serif;
    letter-spacing: -0.005em;
    opacity: 0.58;
  }

  svg { width: 470px; height: auto; }
</style>
<div>
  <h1>${TITLE}</h1>
  <p>${TAGLINE.join("<br>")}</p>
</div>
${mark()}
`;
}

const work = mkdtempSync(resolve(tmpdir(), "rift-og-"));
const html = resolve(work, "card.html");
const shot = resolve(work, "card.png");

writeFileSync(html, card(await Promise.all([webfont("Geist Mono", 400), webfont("Inter", 300)])));

const chrome = findChrome();
const render = spawnSync(
  chrome,
  [
    "--headless",
    "--disable-gpu",
    "--disable-extensions",
    "--hide-scrollbars",
    `--force-device-scale-factor=${SUPERSAMPLE}`,
    `--window-size=${WIDTH},${HEIGHT}`,
    // The fonts are inlined, but the layout still has to settle before the shutter.
    "--virtual-time-budget=2000",
    `--screenshot=${shot}`,
    html,
  ],
  { encoding: "utf8" },
);

if (render.error) throw render.error;
if (!existsSync(shot)) {
  throw new Error(`${chrome} wrote no screenshot:\n${render.stderr ?? ""}`);
}

await sharp(readFileSync(shot)).resize(WIDTH, HEIGHT).png({ compressionLevel: 9 }).toFile(OUT);

const { size } = statSync(OUT);
console.log(`og.png: ${WIDTH}x${HEIGHT}, ${(size / 1024).toFixed(1)} KB, shot with ${chrome}`);
