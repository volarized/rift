'use client';

import { useEffect, useMemo, useRef } from 'react';
import { animated, useReducedMotion, useSprings } from '@react-spring/web';

import { useViewportBloom } from '@/hooks/use-viewport-bloom';
import { cn } from '@/lib/utils';

/**
 * The rift logo: an inverted triangle strung with curve stitching. Each side
 * is subdivided, and every subdivision point is joined to its mirror on the
 * next side, so the straight strings envelope a parabola.
 *
 * One component for every placement — nav, hero, anywhere else. Placements
 * differ only by props, because line weight is not scale-invariant: the mesh
 * that reads as a drawing at 440px silts up into a blob at 22px.
 */
export type LogoProps = Omit<React.ComponentProps<'svg'>, 'children'> & {
  /**
   * Strings per side. 26 is the full mark; drop toward 5-8 for small
   * placements, where a dense mesh fills in solid.
   */
  stitches?: number;
  /**
   * Stroke weight for the strings, in viewBox units — the viewBox is 100 wide,
   * so 0.25 is a quarter of one percent of the mark. It scales with the mark
   * rather than being pinned to a device pixel, so small placements need a much
   * larger number to land on the same drawn thickness: a 224px mark wants ~0.24,
   * a 13px one wants ~4.5.
   */
  strokeWidth?: number;
  /**
   * Stroke weight for the three outer sides. Defaults to `strokeWidth * 1.7` —
   * the outline is the silhouette, and it reads as mush if it carries the same
   * weight as the mesh behind it.
   */
  edgeWidth?: number;
  /** Strings are dimmed against the outline so the triangle stays legible. */
  stringOpacity?: number;
  /** Stitch the mark in on mount, string by string, on a react-spring stagger. */
  animate?: boolean;
  /** Seconds before the first string is drawn. */
  delay?: number;
  /**
   * Stitch the mark to the scroll instead of to mount: it draws itself in as
   * it comes up on the middle of the screen and unpicks itself as it leaves,
   * so scrolling back up undoes it. Takes precedence over `animate`.
   */
  bloom?: boolean;
};

const VIEW_W = 100;

/**
 * Slack around the mark's own bounds, so the outline's stroke — which straddles
 * the boundary and is fattened by a round cap — is not clipped by the viewport.
 */
const PAD = 2;

type Point = { x: number; y: number };

type Segment = {
  from: Point;
  to: Point;
  /**
   * Seconds after `delay` before this string starts drawing, and — scaled to
   * 0..1 — its place in the scroll-driven stagger.
   */
  offset: number;
  /** One of the three outer sides, rather than a string across the interior. */
  edge: boolean;
  /** Drawn length, so a dash pattern can hide and reveal it exactly. */
  length: number;
};

/** Vertices on the unit circle, starting at the bottom apex. */
const VERTICES: Point[] = [0, 1, 2].map((k) => {
  const theta = -Math.PI / 2 + (k / 3) * Math.PI * 2;
  return { x: Math.cos(theta), y: Math.sin(theta) };
});

const Y_TOP = Math.max(...VERTICES.map((v) => v.y));

/** Fills the viewBox on the wider axis. */
const SCALE = VIEW_W / 2 / Math.max(...VERTICES.map((v) => Math.abs(v.x)));

/**
 * The viewBox tracks the mark's own bounds rather than a square, so the
 * element's height *is* the drawn height — a placement can then be sized
 * against a cap height and actually land on it.
 */
const VIEW_H = (Y_TOP - Math.min(...VERTICES.map((v) => v.y))) * SCALE;

/** Unit-circle space to viewBox space. SVG's y axis points down. */
function place(p: Point): Point {
  return {
    x: VIEW_W / 2 + p.x * SCALE,
    y: (Y_TOP - p.y) * SCALE,
  };
}

function lerp(a: Point, b: Point, t: number): Point {
  return { x: a.x + (b.x - a.x) * t, y: a.y + (b.y - a.y) * t };
}

/**
 * How much of the scroll's travel is spent staggering rather than drawing.
 * The last string still reaches full length by the time the mark is centred.
 */
const SPREAD = 0.45;

/** The largest `offset` a string can carry, used to normalise the stagger. */
const LAST = 0.1 + 0.72;

/** How hard the drawn length chases the scroll. Lower is looser. */
const CHASE = 0.16;

const cache = new Map<number, Segment[]>();

function add(
  into: Segment[],
  from: Point,
  to: Point,
  offset: number,
  edge: boolean,
) {
  into.push({
    from,
    to,
    offset,
    edge,
    length: Math.hypot(to.x - from.x, to.y - from.y),
  });
}

function segmentsFor(stitches: number): Segment[] {
  const cached = cache.get(stitches);
  if (cached) return cached;

  const segments: Segment[] = [];

  for (let corner = 0; corner < 3; corner++) {
    const a = VERTICES[corner];
    const b = VERTICES[(corner + 1) % 3];
    const d = VERTICES[(corner + 2) % 3];

    // The outline lands first; the strings fill in behind it.
    add(segments, place(a), place(b), 0, true);

    for (let i = 1; i < stitches; i++) {
      const u = i / stitches;
      add(
        segments,
        place(lerp(a, b, u)),
        place(lerp(a, d, 1 - u)),
        0.1 + u * 0.72,
        false,
      );
    }
  }

  cache.set(stitches, segments);
  return segments;
}

export function Logo({
  stitches = 26,
  strokeWidth = 0.25,
  edgeWidth = strokeWidth * 1.7,
  stringOpacity = 0.62,
  animate = false,
  delay = 0,
  bloom = false,
  className,
  ...props
}: LogoProps) {
  const reduced = useReducedMotion();
  const segments = useMemo(() => segmentsFor(stitches), [stitches]);
  const blooming = bloom && !reduced;
  const stitching = animate && !bloom && !reduced;

  const host = useRef<SVGSVGElement>(null);
  const drawn = useRef<(SVGLineElement | null)[]>([]);
  const target = useViewportBloom(host);

  // Written straight to the strings rather than held in state: eighty dash
  // offsets a frame is nothing, and a render per scroll position is not.
  useEffect(() => {
    const element = host.current;
    if (!blooming || !element) return;

    let frame = 0;
    let live = false;
    let extent = 0;

    const paint = () => {
      extent += (target.current - extent) * CHASE;

      for (let i = 0; i < segments.length; i++) {
        const line = drawn.current[i];
        if (!line) continue;
        const lead = (segments[i].offset / LAST) * SPREAD;
        const drawnTo = Math.min(
          1,
          Math.max(0, (extent - lead) / (1 - SPREAD)),
        );
        line.style.strokeDashoffset = String(segments[i].length * (1 - drawnTo));
      }

      frame = live ? requestAnimationFrame(paint) : 0;
    };

    const observer = new IntersectionObserver(([entry]) => {
      if (entry.isIntersecting === live) return;
      live = entry.isIntersecting;
      if (live && !frame) frame = requestAnimationFrame(paint);
    });
    observer.observe(element);

    return () => {
      live = false;
      if (frame) cancelAnimationFrame(frame);
      observer.disconnect();
    };
  }, [blooming, segments, target]);

  // One spring per string, each on its own delay, so the mark draws itself
  // rather than fading in as a block. Zero springs when it is drawn already.
  const [springs] = useSprings(
    stitching ? segments.length : 0,
    (index) => ({
      from: { extent: 0 },
      to: { extent: 1 },
      delay: (delay + segments[index].offset) * 1000,
      config: { tension: 190, friction: 26, clamp: true },
    }),
    [stitching, segments, delay],
  );

  return (
    <svg
      ref={host}
      viewBox={`${-PAD} ${-PAD} ${VIEW_W + PAD * 2} ${VIEW_H + PAD * 2}`}
      role="img"
      aria-label="rift"
      className={cn('block', className)}
      {...props}
    >
      <g fill="none" stroke="currentColor" strokeLinecap="round">
        {stitching
          ? springs.map((spring, i) => (
              <animated.line
                key={i}
                strokeWidth={segments[i].edge ? edgeWidth : strokeWidth}
                opacity={segments[i].edge ? 1 : stringOpacity}
                x1={segments[i].from.x}
                y1={segments[i].from.y}
                x2={spring.extent.to(
                  (e) =>
                    segments[i].from.x +
                    (segments[i].to.x - segments[i].from.x) * e,
                )}
                y2={spring.extent.to(
                  (e) =>
                    segments[i].from.y +
                    (segments[i].to.y - segments[i].from.y) * e,
                )}
              />
            ))
          : segments.map((segment, i) => (
              <line
                key={i}
                ref={(element) => {
                  drawn.current[i] = element;
                }}
                strokeWidth={segment.edge ? edgeWidth : strokeWidth}
                opacity={segment.edge ? 1 : stringOpacity}
                x1={segment.from.x}
                y1={segment.from.y}
                x2={segment.to.x}
                y2={segment.to.y}
                // A dash as long as the string itself: sliding its offset
                // draws the string on and off without moving its ends.
                strokeDasharray={blooming ? segment.length : undefined}
                strokeDashoffset={blooming ? segment.length : undefined}
              />
            ))}
      </g>
    </svg>
  );
}
