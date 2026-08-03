"use client";

import { animated, useInView, useReducedMotion } from "@react-spring/web";

import { cn } from "@/lib/utils";

/**
 * Entry reveal: opacity 0 to 1 plus a 16px rise as the section comes into
 * view. Content is visible by default at the end of the spring, and skipped
 * entirely under reduced motion — motion never gates reading.
 */
export function Reveal({
  className,
  children,
  ...props
}: React.ComponentProps<typeof animated.div>) {
  const reduced = useReducedMotion();

  const [ref, springs] = useInView(
    () => ({
      from: { opacity: 0, y: 16 },
      to: { opacity: 1, y: 0 },
      immediate: reduced ?? false,
      config: { tension: 170, friction: 26, clamp: true },
    }),
    { rootMargin: "-10% 0px -15% 0px", once: true },
  );

  return (
    <animated.div
      ref={ref}
      className={cn(className)}
      style={{
        opacity: springs.opacity,
        transform: springs.y.to((value) => `translate3d(0, ${value}px, 0)`),
      }}
      {...props}
    >
      {children}
    </animated.div>
  );
}
