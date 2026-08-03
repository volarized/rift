'use client';

import { useEffect, useRef, type RefObject } from 'react';

/**
 * How centred an element is in the viewport, 0 to 1: nothing while it is out
 * near the edges of the screen, everything as it crosses the middle.
 *
 * This is the page's one in-and-out mechanic. Anything driven by it composes
 * on the way in and comes apart again on the way out, rather than playing a
 * one-shot entrance and then sitting finished — scrolling back up undoes it.
 *
 * The value is handed back as a ref, not as state, because every consumer of
 * it reads on an animation frame. A render per scroll position would cost far
 * more than the effect is worth. Pass `onChange` for consumers that have to
 * write to the DOM instead.
 */

/**
 * How far from the centre line, as a fraction of the viewport, the element has
 * to be before it is fully composed. Half a viewport either way.
 */
const REACH = 0.5;

const smooth = (t: number) => t * t * (3 - 2 * t);

export function useViewportBloom(
  // Any element with a box: the SVG marks measure themselves the same way the
  // canvases do.
  ref: RefObject<Element | null>,
  onChange?: (bloom: number) => void,
) {
  const bloom = useRef(0);

  // Held in a ref so a caller may pass an inline callback without the listener
  // being torn down and rebuilt on every render.
  const notify = useRef(onChange);
  notify.current = onChange;

  useEffect(() => {
    const element = ref.current;
    if (!element) return;

    let frame = 0;

    const read = () => {
      frame = 0;

      const rect = element.getBoundingClientRect();
      const viewport = window.innerHeight;
      const centre = rect.top + rect.height / 2;
      const away = Math.abs(centre - viewport / 2) / (viewport * REACH);

      const value = smooth(Math.min(1, Math.max(0, 1 - away)));
      bloom.current = value;
      notify.current?.(value);
    };

    const schedule = () => {
      if (frame) return;
      frame = requestAnimationFrame(read);
    };

    read();
    window.addEventListener('scroll', schedule, { passive: true });
    window.addEventListener('resize', schedule);

    return () => {
      if (frame) cancelAnimationFrame(frame);
      window.removeEventListener('scroll', schedule);
      window.removeEventListener('resize', schedule);
    };
  }, [ref]);

  return bloom;
}
