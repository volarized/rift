'use client';

import { useEffect, useState, type RefObject } from 'react';

type Ink = {
  /** An `rgb()` string, which is all three.js knows how to parse. */
  color: string;
  /** True when the ink is dark, i.e. the page is in light mode. */
  onLight: boolean;
};

const FALLBACK: Ink = { color: 'rgb(201, 201, 196)', onLight: false };

/**
 * The design tokens are authored in oklch, and `getComputedStyle` hands those
 * back as `lab()` — a format three.js silently refuses. Painting one pixel and
 * reading it back gets an exact sRGB triple out of any colour syntax the
 * browser accepts, without hard-coding a second copy of the palette.
 */
function resolve(css: string): Ink | null {
  const canvas = document.createElement('canvas');
  canvas.width = 1;
  canvas.height = 1;

  const context = canvas.getContext('2d', { willReadFrequently: true });
  if (!context) return null;

  // A rejected assignment leaves fillStyle untouched, so seed it with a colour
  // nothing would legitimately resolve to.
  context.fillStyle = '#ff00ff';
  context.fillStyle = css;
  if (context.fillStyle === '#ff00ff') return null;

  context.fillRect(0, 0, 1, 1);
  const [r, g, b] = context.getImageData(0, 0, 1, 1).data;

  return {
    color: `rgb(${r}, ${g}, ${b})`,
    onLight: (0.2126 * r + 0.7152 * g + 0.0722 * b) / 255 < 0.5,
  };
}

/**
 * Resolves `currentColor` on an element and keeps it current across theme
 * flips. The design system's ink is a token, not a constant — light mode
 * inverts it.
 */
export function useInkColor(ref: RefObject<HTMLElement | null>) {
  const [ink, setInk] = useState(FALLBACK);

  useEffect(() => {
    const element = ref.current;
    if (!element) return;

    const read = () => {
      const resolved = resolve(getComputedStyle(element).color);
      if (resolved) setInk(resolved);
    };
    read();

    const observer = new MutationObserver(read);
    observer.observe(document.documentElement, {
      attributes: true,
      attributeFilter: ['class', 'style'],
    });

    return () => observer.disconnect();
  }, [ref]);

  return ink;
}

/**
 * The page's own background, in a form three.js accepts. A solid whose colour
 * matches the void is how a hairline object occludes what sits behind it.
 */
export function useSurfaceColor() {
  const [surface, setSurface] = useState('rgb(21, 21, 24)');

  useEffect(() => {
    const read = () => {
      const resolved = resolve(getComputedStyle(document.body).backgroundColor);
      if (resolved) setSurface(resolved.color);
    };
    read();

    const observer = new MutationObserver(read);
    observer.observe(document.documentElement, {
      attributes: true,
      attributeFilter: ['class', 'style'],
    });

    return () => observer.disconnect();
  }, []);

  return surface;
}

/**
 * Ink on a light page needs more weight than ink on the void: the same alpha
 * that reads as a clean hairline over black nearly vanishes over paper.
 */
export function inkOpacity(base: number, onLight: boolean) {
  return onLight ? Math.min(1, base * 1.5) : base;
}
