"use client";

import { useEffect, useMemo, useRef, useState } from "react";
import { animated, useSpring, type SpringValue } from "@react-spring/three";
import { animated as html, useSpring as useHtmlSpring } from "@react-spring/web";
import { Canvas, useThree } from "@react-three/fiber";
import * as THREE from "three";

import { inkOpacity, useInkColor, useSurfaceColor } from "@/hooks/use-ink-color";
import { useScrollProgress } from "@/hooks/use-scroll-progress";
import { cn } from "@/lib/utils";

/**
 * An exploded isometric stack, played by the scroll. Four plates start nested
 * and separate as the scene scrolls past; the accompanying text sits to the
 * side, each entry lighting up as its plate arrives.
 *
 * The plates are real geometry under an orthographic camera on the isometric
 * diagonal, so nothing here projects by hand — three does the drawing and
 * react-spring does the moving.
 */
export type IsoLayer = {
  label: string;
  note?: string;
};

/** Plate footprint and thickness, in world units. */
const SIDE = 4;
const THICK = 0.26;

/** Vertical pitch between plates, nested through exploded. */
const GAP_MIN = 0.34;
const GAP_MAX = 1.55;

/** Each layer's reveal window, as a fraction of the scene's scroll. */
const REVEAL_START = 0.1;
const REVEAL_PITCH = 0.2;
const REVEAL_RAMP = 0.16;
const REVEAL_END = REVEAL_START + REVEAL_PITCH * 3 + REVEAL_RAMP;

/** World height the camera frames once the stack is fully exploded. */
const FRAME = 7.2;

const clamp = (value: number) => Math.min(1, Math.max(0, value));

function revealAt(progress: number, index: number) {
  return clamp((progress - (REVEAL_START + index * REVEAL_PITCH)) / REVEAL_RAMP);
}

type Rect = { x: number; z: number; w: number; d: number };

/**
 * One motif per layer, so the four plates read as four different things
 * rather than four copies. Files, an agent's panes, a hatched toolkit, output.
 */
const MOTIFS: { rects: Rect[]; hatch?: number }[] = [
  {
    rects: [-1.2, 0, 1.2].flatMap((x) => [-1.2, 0, 1.2].map((z) => ({ x, z, w: 1, d: 1 }))),
  },
  {
    rects: [
      { x: -0.7, z: 0, w: 2.2, d: 3.4 },
      { x: 1.15, z: -0.9, w: 1.1, d: 1.5 },
      { x: 1.15, z: 0.9, w: 1.1, d: 1.5 },
    ],
  },
  { rects: [{ x: 0, z: 0, w: 3.5, d: 3.5 }], hatch: 11 },
  {
    rects: [
      { x: 0, z: -1.05, w: 3.4, d: 1.3 },
      { x: -0.95, z: 0.95, w: 1.5, d: 1.3 },
      { x: 0.95, z: 0.95, w: 1.5, d: 1.3 },
    ],
  },
];

/** Motif outlines as one line-segment geometry, laid on the plate's top face. */
function motifGeometry(index: number) {
  const y = THICK / 2 + 0.002;
  const points: THREE.Vector3[] = [];

  const edge = (x1: number, z1: number, x2: number, z2: number) => {
    points.push(new THREE.Vector3(x1, y, z1), new THREE.Vector3(x2, y, z2));
  };

  for (const { x, z, w, d } of MOTIFS[index].rects) {
    const [x0, x1, z0, z1] = [x - w / 2, x + w / 2, z - d / 2, z + d / 2];
    edge(x0, z0, x1, z0);
    edge(x1, z0, x1, z1);
    edge(x1, z1, x0, z1);
    edge(x0, z1, x0, z0);
  }

  const hatch = MOTIFS[index].hatch;
  if (hatch) {
    const { x, z, w, d } = MOTIFS[index].rects[0];
    for (let i = 1; i < hatch; i++) {
      const at = x - w / 2 + (i / hatch) * w;
      edge(at, z - d / 2, at, z + d / 2);
    }
  }

  return new THREE.BufferGeometry().setFromPoints(points);
}

/**
 * A ruled field over the whole top face, under the motif. Nothing to read on
 * its own — it just gives the plate a surface, so the solid reads as drawn
 * rather than as a slab with a diagram on it.
 */
const RULES = 22;

function ruleGeometry() {
  const y = THICK / 2 + 0.001;
  const half = SIDE / 2;
  const points: THREE.Vector3[] = [];

  for (let i = 1; i < RULES; i++) {
    const at = -half + (i / RULES) * SIDE;
    points.push(
      new THREE.Vector3(at, y, -half),
      new THREE.Vector3(at, y, half),
      new THREE.Vector3(-half, y, at),
      new THREE.Vector3(half, y, at),
    );
  }

  return new THREE.BufferGeometry().setFromPoints(points);
}

function Plate({
  index,
  count,
  open,
  ink,
  surface,
  boost,
}: {
  index: number;
  count: number;
  open: SpringValue<number>;
  ink: string;
  surface: string;
  boost: number;
}) {
  const box = useMemo(() => new THREE.BoxGeometry(SIDE, THICK, SIDE), []);
  const edges = useMemo(() => new THREE.EdgesGeometry(box), [box]);
  const motif = useMemo(() => motifGeometry(index), [index]);
  const rules = useMemo(() => ruleGeometry(), []);

  useEffect(
    () => () => {
      box.dispose();
      edges.dispose();
      motif.dispose();
      rules.dispose();
    },
    [box, edges, motif, rules],
  );

  const centre = (count - 1) / 2;
  const lift = (value: number) => (centre - index) * (GAP_MIN + (GAP_MAX - GAP_MIN) * value);
  const alpha = (value: number) => (0.24 + 0.64 * revealAt(value, index)) * boost;

  return (
    <animated.group position-y={open.to(lift)}>
      {/* A solid in the page's own colour: the plate occludes what's behind it. */}
      <mesh geometry={box}>
        <meshBasicMaterial color={surface} />
      </mesh>
      <lineSegments geometry={edges}>
        <animated.lineBasicMaterial color={ink} transparent opacity={open.to(alpha)} />
      </lineSegments>
      <lineSegments geometry={rules}>
        <animated.lineBasicMaterial
          color={ink}
          transparent
          opacity={open.to((value) => alpha(value) * 0.16)}
        />
      </lineSegments>
      <lineSegments geometry={motif}>
        <animated.lineBasicMaterial
          color={ink}
          transparent
          opacity={open.to((value) => alpha(value) * 0.6)}
        />
      </lineSegments>
    </animated.group>
  );
}

/** Keeps the whole exploded stack framed, whatever the canvas ends up being. */
function Frame() {
  const camera = useThree((state) => state.camera);
  const size = useThree((state) => state.size);

  useEffect(() => {
    const orthographic = camera as THREE.OrthographicCamera;
    orthographic.zoom = Math.min(size.width / FRAME, size.height / FRAME);
    orthographic.updateProjectionMatrix();
  }, [camera, size]);

  return null;
}

export function IsoStack({
  layers,
  heading,
  className,
}: {
  layers: IsoLayer[];
  /**
   * Section heading, set above the layer list in the side column. Mobile has
   * no room for it beside the scene, so the caller hides it there.
   */
  heading?: React.ReactNode;
  className?: string;
}) {
  const scene = useRef<HTMLDivElement>(null);
  const host = useRef<HTMLDivElement>(null);
  const scrolled = useScrollProgress(scene);
  const { color, onLight } = useInkColor(host);
  const surface = useSurfaceColor();
  const [still, setStill] = useState(false);

  useEffect(() => {
    const motion = window.matchMedia("(prefers-reduced-motion: reduce)");
    const sync = () => setStill(motion.matches);
    sync();
    motion.addEventListener("change", sync);
    return () => motion.removeEventListener("change", sync);
  }, []);

  // Reduced motion gets the finished state: fully exploded, every label read.
  const progress = still ? 1 : scrolled;
  const config = { tension: 120, friction: 30, clamp: true };

  const { open } = useSpring({ open: progress, immediate: still, config });
  const { read } = useHtmlSpring({ read: progress, immediate: still, config });

  return (
    <div ref={scene} className={cn(still ? "" : "h-[300vh]", className)}>
      <div
        className={cn(
          // One column on mobile — heading and rail first, scene taking
          // whatever height is left — and two side by side from md up.
          "relative flex flex-col gap-6 md:grid md:grid-cols-2 md:items-start md:gap-14",
          still ? "py-16" : "sticky top-14 h-[calc(100svh-3.5rem)] py-8 md:py-10",
        )}
      >
        {/* Plotting-paper ground: a dot grid under the scene half only, ruled
            off from the text by a single hairline and bled to the viewport. */}
        <div
          aria-hidden
          className="pointer-events-none absolute inset-y-0 right-[-50vw] left-[-50vw] bg-[radial-gradient(circle,var(--border)_1px,transparent_1px)] bg-size-[26px_26px] md:left-1/2 md:border-l md:border-border"
        />

        {/* The accompanying text: set to the side, aligned top and left. */}
        <div className="relative grid shrink-0 content-start justify-items-start gap-6 text-left md:gap-8 md:pt-6">
          {heading}
          {/* A rail with a node per layer: one continuous line down the
              column, each node filling in as its plate arrives. Order is
              carried by the scroll, so the entries are not numbered. */}
          <ol className="relative m-0 grid w-full list-none gap-7 p-0 md:gap-16">
            <span
              aria-hidden
              className="pointer-events-none absolute inset-y-0 left-[6.5px] w-px bg-border"
            />
            {layers.map((layer, index) => (
              <html.li
                key={layer.label}
                className="relative grid grid-cols-[auto_1fr] items-start gap-5"
                style={{
                  opacity: read.to((value) => 0.3 + 0.7 * clamp(revealAt(value, index) * 1.5)),
                }}
              >
                {/* Opaque, so the node reads as a break in the rail. */}
                <span className="mt-0.75 grid size-3.5 place-items-center rounded-full border border-border bg-background">
                  <html.span
                    className="size-1.5 rounded-full bg-foreground"
                    style={{
                      opacity: read.to((value) => clamp(revealAt(value, index) * 1.5)),
                    }}
                  />
                </span>
                <div className="grid gap-1.5">
                  <span className="font-mono text-[11px] tracking-[0.14em] text-foreground uppercase">
                    {layer.label}
                  </span>
                  {layer.note ? (
                    <p className="m-0 text-[13px] leading-snug font-light text-pretty text-muted-foreground">
                      {layer.note}
                    </p>
                  ) : null}
                </div>
              </html.li>
            ))}
          </ol>
        </div>

        <div ref={host} className="relative min-h-0 flex-1 text-foreground md:h-full">
          <Canvas
            orthographic
            // No tone mapping: the occluder has to come out the exact colour
            // of the page, or the plates read as grey slabs on a light theme.
            flat
            dpr={[1, 2]}
            gl={{ antialias: true, alpha: true }}
            camera={{ position: [8, 7, 8], near: -60, far: 120, zoom: 60 }}
          >
            <Frame />
            {layers.map((layer, index) => (
              <Plate
                key={layer.label}
                index={index}
                count={layers.length}
                open={open}
                ink={color}
                surface={surface}
                boost={inkOpacity(1, onLight)}
              />
            ))}
          </Canvas>
        </div>
      </div>
    </div>
  );
}
