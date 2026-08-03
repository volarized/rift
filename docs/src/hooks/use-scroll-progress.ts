"use client";

import { type RefObject, useEffect, useState } from "react";

/**
 * How far the viewport has travelled through a tall element, 0 to 1 — the
 * driver for a sticky scene that plays as you scroll past it.
 *
 * Reads are batched onto one animation frame, and the value is quantised to a
 * thousandth so a scroll only re-renders when the scene would actually move.
 */
export function useScrollProgress(ref: RefObject<HTMLElement | null>) {
  const [progress, setProgress] = useState(0);

  useEffect(() => {
    const element = ref.current;
    if (!element) return;

    let frame = 0;

    const read = () => {
      frame = 0;
      const { top, height } = element.getBoundingClientRect();
      // Anything shorter than the viewport never scrolls through: it is either
      // still ahead of us or already behind.
      const travel = height - window.innerHeight;
      if (travel <= 0) {
        setProgress(top <= 0 ? 1 : 0);
        return;
      }
      const raw = Math.min(1, Math.max(0, -top / travel));
      setProgress(Math.round(raw * 1000) / 1000);
    };

    const schedule = () => {
      if (frame) return;
      frame = requestAnimationFrame(read);
    };

    read();
    window.addEventListener("scroll", schedule, { passive: true });
    window.addEventListener("resize", schedule);

    return () => {
      if (frame) cancelAnimationFrame(frame);
      window.removeEventListener("scroll", schedule);
      window.removeEventListener("resize", schedule);
    };
  }, [ref]);

  return progress;
}
