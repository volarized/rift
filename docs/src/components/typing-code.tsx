"use client";

import { animate, useReducedMotion } from "motion/react";
import { useLayoutEffect, useRef } from "react";

export function TypingCode({ source }: { source: string }) {
  const code = useRef<HTMLElement>(null);
  const prefersReducedMotion = useReducedMotion();

  useLayoutEffect(() => {
    const node = code.current;
    if (!node || !source || prefersReducedMotion) return;

    const characters = Array.from(source);
    node.textContent = "";
    const animation = animate(0, characters.length, {
      duration: 0.5,
      ease: [0.22, 1, 0.36, 1],
      onUpdate: (count) => {
        node.textContent = characters.slice(0, Math.floor(count)).join("");
      },
      onComplete: () => {
        node.textContent = source;
      },
    });
    return () => {
      animation.stop();
      node.textContent = source;
    };
  }, [source, prefersReducedMotion]);

  return (
    <>
      <span className="sr-only select-none">{source}</span>
      <code aria-hidden="true" ref={code}>
        {source}
      </code>
    </>
  );
}
