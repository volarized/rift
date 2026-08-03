'use client';

import { useEffect, useMemo, useRef, useState } from 'react';
import { Canvas, useFrame, useThree } from '@react-three/fiber';
import * as THREE from 'three';

import { inkOpacity, useInkColor } from '@/hooks/use-ink-color';
import { useViewportBloom } from '@/hooks/use-viewport-bloom';
import { cn } from '@/lib/utils';

/**
 * Line objects — the design system's only graphic. Strange attractors, wireframe
 * topologies and lofted sweeps, drawn as continuous three.js hairlines in ink
 * over the void. No glow, no bloom, no fill, no colour. One object per section.
 */
export type LineObjectKind =
  | 'aizawa'
  | 'thomas'
  | 'halvorsen'
  | 'lorenz'
  | 'rikitake'
  | 'shimizu'
  | 'tsucs1'
  | 'triangle'
  | 'loft'
  | 'globe'
  | 'lens'
  | 'contour'
  | 'terrain';

type Vec3 = [number, number, number];
type Polyline = Vec3[];

const FOV = 42;
const TAU = Math.PI * 2;

/**
 * A fan of trajectories rather than one. The seeds sit a hair apart; chaos
 * separates them within a couple of turns and each lays down its own filament,
 * so the attractor reads as woven shading instead of a single hard wire.
 */
function integrate(
  derivative: (x: number, y: number, z: number) => Vec3,
  start: Vec3,
  dt: number,
  steps: number,
  strands = 1,
  spread = 0.012,
): Polyline[] {
  const lines: Polyline[] = [];

  for (let s = 0; s < strands; s++) {
    // Deterministic fan — golden-angle offsets, so no two seeds share a
    // direction and the build stays identical between renders.
    const angle = s * 0.6180339887 * TAU;
    const offset = spread * s;
    let x = start[0] + offset * Math.cos(angle);
    let y = start[1] + offset * Math.sin(angle);
    let z = start[2] + offset * Math.cos(angle * 1.7);

    const points: Polyline = [];
    for (let i = 0; i < steps; i++) {
      const [dx, dy, dz] = derivative(x, y, z);
      x += dx * dt;
      y += dy * dt;
      z += dz * dt;
      // Drop the transient before the trajectory settles onto the attractor.
      if (i > 200) points.push([x, y, z]);
    }
    lines.push(points);
  }

  return lines;
}

function build(kind: LineObjectKind): Polyline[] {
  if (kind === 'aizawa') {
    const a = 0.95,
      b = 0.7,
      c = 0.6,
      d = 3.5,
      e = 0.25,
      f = 0.1;
    return integrate(
      (x, y, z) => [
        (z - b) * x - d * y,
        d * x + (z - b) * y,
        c +
          a * z -
          (z * z * z) / 3 -
          (x * x + y * y) * (1 + e * z) +
          f * z * x * x * x,
      ],
      [0.1, 0, 0],
      0.004,
      17000,
      9,
    );
  }

  if (kind === 'thomas') {
    const b = 0.19;
    return integrate(
      (x, y, z) => [
        Math.sin(y) - b * x,
        Math.sin(z) - b * y,
        Math.sin(x) - b * z,
      ],
      [1.1, 1.1, -0.01],
      0.02,
      17000,
      7,
    );
  }

  if (kind === 'halvorsen') {
    const a = 1.4;
    return integrate(
      (x, y, z) => [
        -a * x - 4 * y - 4 * z - y * y,
        -a * y - 4 * z - 4 * x - z * z,
        -a * z - 4 * x - 4 * y - x * x,
      ],
      [-1.48, -1.51, 2.04],
      0.003,
      18000,
      7,
    );
  }

  if (kind === 'tsucs1') {
    // Three-Scroll Unified Chaotic System, type 1. Fast dynamics — the step
    // has to stay small or Euler walks straight off the attractor.
    const a = 40,
      c = 0.833,
      d = 0.5,
      e = 0.65,
      f = 20;
    return integrate(
      (x, y, z) => [
        a * (y - x) + d * x * z,
        f * y - x * z,
        c * z + x * y - e * x * x,
      ],
      [1, 1, 1],
      0.0006,
      30000,
      7,
    );
  }

  if (kind === 'rikitake') {
    // The two-disc dynamo: a pair of scrolls the trajectory swaps between,
    // which is what a field reversal looks like as a curve.
    const mu = 1,
      a = 5;
    return integrate(
      (x, y, z) => [-mu * x + z * y, -mu * y + (z - a) * x, 1 - x * y],
      [-1.4, -1, -1],
      0.004,
      19000,
      8,
    );
  }

  if (kind === 'shimizu') {
    const a = 0.75,
      b = 0.45;
    return integrate(
      (x, y, z) => [y, x * (1 - z) - a * y, x * x - b * z],
      [0.6, 0, 0],
      0.006,
      20000,
      8,
    );
  }

  if (kind === 'lorenz') {
    const sigma = 10,
      rho = 28,
      beta = 8 / 3;
    return integrate(
      (x, y, z) => [sigma * (y - x), x * (rho - z) - y, x * y - beta * z],
      [0.01, 0, 0],
      0.0032,
      20000,
      7,
    );
  }

  // The wireframe topologies. Parallels, meridians, contours and a warped
  // ground plane, all traced off three's own curve primitives.
  if (kind === 'globe') {
    const lines: Polyline[] = [];
    const BANDS = 15;

    for (let i = 1; i < BANDS; i++) {
      const phi = -Math.PI / 2 + (i / BANDS) * Math.PI;
      const radius = Math.cos(phi);
      const parallel = new THREE.EllipseCurve(0, 0, radius, radius, 0, TAU);
      lines.push(
        parallel.getPoints(96).map((p) => [p.x, Math.sin(phi), p.y] as Vec3),
      );
    }

    const meridian = new THREE.EllipseCurve(0, 0, 1, 1, 0, TAU);
    const ring = meridian.getPoints(96);
    for (let i = 0; i < BANDS; i++) {
      const lon = (i / BANDS) * Math.PI;
      lines.push(
        ring.map(
          (p) => [p.x * Math.cos(lon), p.y, p.x * Math.sin(lon)] as Vec3,
        ),
      );
    }

    return lines;
  }

  if (kind === 'lens') {
    // A family of ellipses collapsing through the edge-on plane and back.
    const COUNT = 42;
    const lines: Polyline[] = [];

    for (let i = 0; i <= COUNT; i++) {
      const t = i / COUNT;
      const curve = new THREE.EllipseCurve(
        0,
        0,
        1,
        Math.abs(1 - 2 * t),
        0,
        TAU,
        false,
        -0.34 + (t - 0.5) * 0.26,
      );
      const depth = (t - 0.5) * 0.24;
      lines.push(curve.getPoints(128).map((p) => [p.x, p.y, depth] as Vec3));
    }

    return lines;
  }

  if (kind === 'contour') {
    // A survey nest: radius grows monotonically per level and the harmonics
    // drift only slightly, so rings deform without ever crossing.
    const RINGS = 16;
    const lines: Polyline[] = [];

    for (let level = 0; level < RINGS; level++) {
      const base = 0.11 + level * 0.058;
      const phase = 0.7 + level * 0.05;
      const control: THREE.Vector3[] = [];

      for (let k = 0; k < 28; k++) {
        const theta = (k / 28) * TAU;
        const shape =
          1 +
          0.24 * Math.sin(3 * theta + phase) +
          0.11 * Math.sin(5 * theta - phase * 1.6) +
          0.05 * Math.sin(8 * theta + phase * 0.4);
        control.push(
          new THREE.Vector3(
            Math.cos(theta) * base * shape * 1.2,
            Math.sin(theta) * base * shape * 0.86,
            // Each level sits a step lower: the nest reads as relief.
            -level * 0.026,
          ),
        );
      }

      const curve = new THREE.CatmullRomCurve3(control, true, 'centripetal');
      lines.push(curve.getPoints(180).map((p) => [p.x, p.y, p.z] as Vec3));
    }

    return lines;
  }

  if (kind === 'terrain') {
    const N = 26;
    const height = (u: number, v: number) =>
      0.3 * Math.sin(u * Math.PI * 2.2) * Math.cos(v * Math.PI * 1.6);
    const lines: Polyline[] = [];

    for (let i = 0; i <= N; i++) {
      const a = i / N;
      const row: Polyline = [];
      const column: Polyline = [];
      for (let k = 0; k <= N; k++) {
        const b = k / N;
        row.push([(a - 0.5) * 2, height(a, b), (b - 0.5) * 2]);
        column.push([(b - 0.5) * 2, height(b, a), (a - 0.5) * 2]);
      }
      lines.push(row, column);
    }

    return lines;
  }

  if (kind === 'triangle') {
    // Nested triangles spiralling inward — the mark, extruded.
    const rings = 54;
    const lines: Polyline[] = [];
    for (let i = 0; i < rings; i++) {
      const t = i / rings;
      const scale = 1 - t * 0.985;
      const rotation = t * 3.4;
      const lift = (t - 0.5) * 0.55;
      const ring: Polyline = [];
      for (let k = 0; k <= 3; k++) {
        const theta = (k / 3) * Math.PI * 2 + rotation - Math.PI / 2;
        ring.push([Math.cos(theta) * scale, Math.sin(theta) * scale, lift]);
      }
      lines.push(ring);
    }
    return lines;
  }

  // loft — a rippled profile swept and twisted down a taper.
  const rings = 46;
  const segments = 150;
  const lines: Polyline[] = [];
  for (let i = 0; i < rings; i++) {
    const t = i / rings;
    const scale = Math.pow(1 - t, 0.72);
    const twist = t * 2.6;
    const ring: Polyline = [];
    for (let k = 0; k <= segments; k++) {
      const theta = (k / segments) * Math.PI * 2;
      const r =
        (1 +
          0.3 * Math.sin(3 * theta + twist) +
          0.12 * Math.sin(5 * theta - twist * 1.7)) *
        scale;
      ring.push([
        r * Math.cos(theta + twist * 0.5),
        r * Math.sin(theta + twist * 0.5),
        (t - 0.5) * 1.5 + 0.25 * Math.sin(2 * theta + twist),
      ]);
    }
    lines.push(ring);
  }
  return lines;
}

/** Integrating an attractor is not cheap; do it once per kind per session. */
const geometryCache = new Map<
  LineObjectKind,
  { lines: Polyline[]; radius: number }
>();

function geometryFor(kind: LineObjectKind) {
  const cached = geometryCache.get(kind);
  if (cached) return cached;

  const lines = build(kind);
  const box = new THREE.Box3();
  const point = new THREE.Vector3();
  for (const line of lines) {
    for (const p of line) box.expandByPoint(point.set(p[0], p[1], p[2]));
  }

  // Recentre on the bounding box so the object orbits its own middle.
  const centre = box.getCenter(new THREE.Vector3());
  for (const line of lines) {
    for (const p of line) {
      p[0] -= centre.x;
      p[1] -= centre.y;
      p[2] -= centre.z;
    }
  }

  const built = {
    lines,
    radius: box.getSize(new THREE.Vector3()).length() / 2,
  };
  geometryCache.set(kind, built);
  return built;
}

/** How hard the drawn length chases the scroll. Lower is looser. */
const CHASE = 0.1;

type ObjectProps = {
  kind: LineObjectKind;
  speed: number;
  tilt: number;
  yaw: number;
  spin: number;
  opacity: number;
  color: string;
  still: boolean;
  /** When set, the object draws itself along the scroll instead of turning. */
  bloom?: React.RefObject<number>;
};

function Lines({
  kind,
  speed,
  tilt,
  yaw,
  spin,
  opacity,
  color,
  still,
  bloom,
}: ObjectProps) {
  const holder = useRef<THREE.Group>(null);
  const invalidate = useThree((state) => state.invalidate);

  const object = useMemo(() => {
    const { lines } = geometryFor(kind);
    const material = new THREE.LineBasicMaterial({ transparent: true });
    const group = new THREE.Group();
    for (const line of lines) {
      const geometry = new THREE.BufferGeometry().setFromPoints(
        line.map((p) => new THREE.Vector3(p[0], p[1], p[2])),
      );
      group.add(new THREE.Line(geometry, material));
    }
    return { group, material };
  }, [kind]);

  useEffect(() => {
    object.material.color.set(color);
    object.material.opacity = opacity;
    // On a demand frameloop nothing repaints on its own after a theme flip.
    invalidate();
  }, [object, color, opacity, invalidate]);

  useEffect(
    () => () => {
      object.material.dispose();
      object.group.traverse((child) => {
        if (child instanceof THREE.Line) child.geometry.dispose();
      });
    },
    [object],
  );

  // How much of the trace is currently drawn, chased towards the scroll.
  const extent = useRef(still ? 1 : 0);

  useFrame(({ clock }) => {
    const group = holder.current;
    if (!group) return;

    if (bloom) {
      // Drawn on by the scroll rather than turned: the trace runs on as the
      // object comes up on the middle of the screen, and back off as it goes.
      extent.current += ((still ? 1 : bloom.current) - extent.current) * CHASE;
      for (const child of object.group.children) {
        if (!(child instanceof THREE.Line)) continue;
        const count = child.geometry.getAttribute('position').count;
        child.geometry.setDrawRange(0, Math.round(count * extent.current));
      }
      // Held still: a drawn-on object is read, not watched.
      group.rotation.set(tilt, yaw, spin);
      return;
    }

    const t = still ? 0 : clock.getElapsedTime();

    if (kind === 'triangle') {
      group.rotation.z = t * speed;
      group.rotation.y = Math.sin(t * speed * 0.8) * 0.3;
      group.rotation.x = Math.sin(t * speed * 0.55) * 0.18;
    } else {
      group.rotation.y = t * speed;
      group.rotation.x = tilt + Math.sin(t * speed * 0.55) * 0.14;
    }
  });

  return (
    <group ref={holder}>
      <primitive object={object.group} />
    </group>
  );
}

export function LineObject({
  kind,
  speed = 0.12,
  tilt = 0.2,
  yaw = 0,
  spin = 0,
  fit = 1,
  opacity = 0.32,
  reveal = false,
  className,
}: {
  kind: LineObjectKind;
  /** Rotation in rad/s. The design keeps this between 0.09 and 0.16. */
  speed?: number;
  /** Resting X rotation, before the drift. */
  tilt?: number;
  /** Y rotation. Only read under `reveal`, where the object is held still. */
  yaw?: number;
  /** Z rotation — the in-plane spin. Only read under `reveal`. */
  spin?: number;
  /** Above 1 pulls the camera in. */
  fit?: number;
  /** Stroke alpha. The design keeps this between 0.30 and 0.42. */
  opacity?: number;
  /**
   * Draw the object on with the scroll instead of turning it: the trace runs
   * on as the object crosses the middle of the screen and back off as it
   * leaves, so scrolling back up undoes it. `speed` is ignored.
   */
  reveal?: boolean;
  className?: string;
}) {
  const host = useRef<HTMLDivElement>(null);
  const { color, onLight } = useInkColor(host);
  const bloom = useViewportBloom(host);
  const [inView, setInView] = useState(true);
  const [still, setStill] = useState(false);

  useEffect(() => {
    const query = window.matchMedia('(prefers-reduced-motion: reduce)');
    const sync = () => setStill(query.matches);
    sync();
    query.addEventListener('change', sync);
    return () => query.removeEventListener('change', sync);
  }, []);

  useEffect(() => {
    const element = host.current;
    if (!element) return;

    const observer = new IntersectionObserver(
      ([entry]) => setInView(entry.isIntersecting),
      { rootMargin: '150px 0px' },
    );
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  // Frame the bounding sphere, then bias per shape — as the design does.
  const distance = useMemo(() => {
    const { radius } = geometryFor(kind);
    return (
      ((radius / Math.sin(((FOV * Math.PI) / 180) / 2)) *
        (kind === 'triangle' ? 1 : 0.72)) /
      fit
    );
  }, [kind, fit]);

  return (
    <div ref={host} className={cn('text-foreground', className)}>
      <Canvas
        frameloop={inView && !still ? 'always' : 'demand'}
        // The ink is a design token; tone mapping would repaint it.
        flat
        dpr={[1, 2]}
        gl={{ antialias: true, alpha: true }}
        // The attractors are built at their own scale — some are a couple of
        // units across, some are seventy — so the clip planes are set off the
        // camera's own distance rather than fixed.
        camera={{
          fov: FOV,
          near: distance / 100,
          far: distance * 4,
          position: [0, 0, distance],
        }}
      >
        <Lines
          kind={kind}
          speed={speed}
          tilt={tilt}
          yaw={yaw}
          spin={spin}
          opacity={inkOpacity(opacity, onLight)}
          color={color}
          still={still}
          bloom={reveal ? bloom : undefined}
        />
      </Canvas>
    </div>
  );
}
