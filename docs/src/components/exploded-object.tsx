"use client";

import { Canvas, useFrame, useThree } from "@react-three/fiber";
import { useEffect, useMemo, useRef, useState } from "react";
import * as THREE from "three";

import { inkOpacity, useInkColor } from "@/hooks/use-ink-color";
import { useViewportBloom } from "@/hooks/use-viewport-bloom";
import { cn } from "@/lib/utils";

/**
 * Wireframe solids taken apart, and put back together, by the scroll.
 *
 * Each object is built as a set of pieces, every one carrying the direction it
 * travels when the assembly opens. A piece is real geometry — the sphere's
 * shell is genuinely cut at the plane, the torus genuinely severed between
 * sectors — so the section faces the drawing shows are the faces that would
 * actually be there.
 *
 * The opening is not a scroll animation that plays once. It tracks how close
 * the object is to the middle of the viewport: composed on the way in, fully
 * apart as it passes the centre line, composed again on the way out.
 */
export type ExplodedKind = "sphere" | "torus" | "geodesic";

const TAU = Math.PI * 2;

type Piece = {
  /** The piece's own wireframe, as line-segment pairs. */
  shell: Float32Array;
  /** Where the solid was cut, drawn heavier. Empty when nothing was cut. */
  cut: Float32Array;
  /** Unit direction the piece travels as the assembly opens. */
  direction: [number, number, number];
  /**
   * Where the piece's middle sits on the cut axis. Set on a slice, which holds
   * its place and thins about that middle instead of travelling — the gaps
   * open between the slices without the solid ever moving or changing size.
   */
  pivot?: number;
};

type Assembly = {
  pieces: Piece[];
  /** The assembly's own diameter, closed. The view adds the separation. */
  frame: number;
  /** How many separations wider the assembly gets, opened. Nil if it thins. */
  spread: number;
};

const push = (out: number[], a: THREE.Vector3, b: THREE.Vector3) => {
  out.push(a.x, a.y, a.z, b.x, b.y, b.z);
};

/* -------------------------------------------------------------------------
 * Sphere — whole for three quarters of its width, then sectioned: ten thin
 * slices and the cap, drawn off the body along the axis they were cut on.
 * ---------------------------------------------------------------------- */

type Plane = { normal: THREE.Vector3; at: number };
type Seg = [THREE.Vector3, THREE.Vector3];

/** One axis, and only one: the slices part sideways, never crosswise. */
const AXIS = new THREE.Vector3(1, 0, 0);

/** Where the body ends — three quarters of the way across the diameter. */
const SPLIT = 0.5;
/** Where the cap begins. Past this the shell closes and stops being a slice. */
const CAP_AT = 0.93;
const SLICES = 10;

const CUTS: Plane[] = [
  { normal: AXIS, at: SPLIT },
  ...Array.from({ length: SLICES }, (_, i) => ({
    normal: AXIS,
    at: SPLIT + ((i + 1) * (CAP_AT - SPLIT)) / SLICES,
  })),
];

/** The body, then a slice per cut, then the cap. */
const PIECES = CUTS.length + 1;

/** Split a run of segments so each one lies wholly on one side of the plane. */
function slice(segments: Seg[], plane: Plane): Seg[] {
  const out: Seg[] = [];

  for (const [a, b] of segments) {
    const sa = a.dot(plane.normal) - plane.at;
    const sb = b.dot(plane.normal) - plane.at;

    if (sa * sb >= 0) {
      out.push([a, b]);
      continue;
    }

    // The crossing point is shared by both halves, so the pieces meet exactly
    // along the rim rather than stepping across it.
    const mid = new THREE.Vector3().lerpVectors(a, b, sa / (sa - sb));
    out.push([a, mid], [mid, b]);
  }

  return out;
}

/** Which piece a segment falls in: how many of the parallel cuts it sits past. */
function pieceOf(segment: Seg) {
  const along = (segment[0].dot(AXIS) + segment[1].dot(AXIS)) / 2;

  let piece = 0;
  for (const cut of CUTS) if (along > cut.at) piece++;
  return piece;
}

function flatten(segments: Seg[]) {
  const out: number[] = [];
  for (const [a, b] of segments) push(out, a, b);
  return Float32Array.from(out);
}

function sphereAssembly(): Assembly {
  const PARALLELS = 26;
  const MERIDIANS = 30;
  const RESOLUTION = 128;

  const lines: THREE.Vector3[][] = [];

  for (let i = 1; i < PARALLELS; i++) {
    const phi = -Math.PI / 2 + (i / PARALLELS) * Math.PI;
    const radius = Math.cos(phi);
    const y = Math.sin(phi);
    const ring: THREE.Vector3[] = [];
    for (let k = 0; k <= RESOLUTION; k++) {
      const t = (k / RESOLUTION) * TAU;
      ring.push(new THREE.Vector3(radius * Math.cos(t), y, radius * Math.sin(t)));
    }
    lines.push(ring);
  }

  for (let m = 0; m < MERIDIANS; m++) {
    const lon = (m / MERIDIANS) * Math.PI;
    const ring: THREE.Vector3[] = [];
    for (let k = 0; k <= RESOLUTION; k++) {
      const t = (k / RESOLUTION) * TAU;
      ring.push(
        new THREE.Vector3(Math.cos(t) * Math.cos(lon), Math.sin(t), Math.cos(t) * Math.sin(lon)),
      );
    }
    lines.push(ring);
  }

  let shell: Seg[] = [];
  for (const line of lines) {
    for (let i = 0; i < line.length - 1; i++) shell.push([line[i], line[i + 1]]);
  }
  for (const plane of CUTS) shell = slice(shell, plane);

  const shells: Seg[][] = Array.from({ length: PIECES }, () => []);
  for (const segment of shell) shells[pieceOf(segment)].push(segment);

  // Each cut is shown by both pieces it lies between: both were sectioned here.
  const faces: Seg[][] = Array.from({ length: PIECES }, () => []);
  CUTS.forEach((plane, index) => {
    const face = disc(
      plane.normal.clone().multiplyScalar(plane.at),
      plane.normal,
      Math.sqrt(Math.max(0, 1 - plane.at * plane.at)),
      4,
    );
    faces[index].push(...face);
    faces[index + 1].push(...face);
  });

  // Nothing here moves. The body is whole and stays whole; every section past
  // it thins about its own middle, so the cuts open as gaps in a sphere that
  // is always the same sphere in the same place.
  const edges = [-1, ...CUTS.map((cut) => cut.at), 1];

  return {
    frame: 2.05,
    spread: 0,
    pieces: shells.map((segments, index) => ({
      shell: flatten(segments),
      cut: flatten(faces[index]),
      direction: [0, 0, 0] as [number, number, number],
      // The body is the one piece that keeps its full width.
      pivot: index === 0 ? undefined : (edges[index] + edges[index + 1]) / 2,
    })),
  };
}

/** Concentric rings on a plane — how a section face is drawn. */
function disc(centre: THREE.Vector3, normal: THREE.Vector3, radius: number, rings: number): Seg[] {
  // Any vector off the normal will do to start the basis the disc is drawn in.
  const helper = Math.abs(normal.y) < 0.9 ? new THREE.Vector3(0, 1, 0) : new THREE.Vector3(1, 0, 0);
  const u = new THREE.Vector3().crossVectors(helper, normal).normalize();
  const v = new THREE.Vector3().crossVectors(normal, u);

  const out: Seg[] = [];
  const RESOLUTION = 96;

  for (let ring = 1; ring <= rings; ring++) {
    const r = radius * (ring / rings);
    let previous = new THREE.Vector3();
    for (let k = 0; k <= RESOLUTION; k++) {
      const t = (k / RESOLUTION) * TAU;
      const point = new THREE.Vector3()
        .copy(centre)
        .addScaledVector(u, Math.cos(t) * r)
        .addScaledVector(v, Math.sin(t) * r);
      if (k > 0) out.push([previous, point]);
      previous = point;
    }
  }

  return out;
}

/* -------------------------------------------------------------------------
 * Torus — a ring severed into sectors that open outwards like a cut band.
 * ---------------------------------------------------------------------- */

function torusAssembly(): Assembly {
  const SECTORS = 5;
  const MAJOR = 0.74;
  const MINOR = 0.28;
  /** Tube cross-sections per sector, and points around each cross-section. */
  const RINGS = 24;
  const AROUND = 46;

  const at = (u: number, v: number) => {
    const cu = Math.cos(u);
    const su = Math.sin(u);
    const r = MAJOR + MINOR * Math.cos(v);
    return new THREE.Vector3(r * cu, MINOR * Math.sin(v), r * su);
  };

  const pieces: Piece[] = [];
  const span = TAU / SECTORS;
  // A gap in the sweep, so a sector is a cut piece of ring rather than a
  // seamless fifth of one.
  const inset = span * 0.045;

  for (let sector = 0; sector < SECTORS; sector++) {
    const start = sector * span + inset;
    const end = (sector + 1) * span - inset;
    const shell: number[] = [];

    // Cross-sections along the sector.
    for (let i = 0; i <= RINGS; i++) {
      const u = start + ((end - start) * i) / RINGS;
      for (let k = 0; k < AROUND; k++) {
        push(shell, at(u, (k / AROUND) * TAU), at(u, ((k + 1) / AROUND) * TAU));
      }
    }

    // Lines running the length of the sector, one per cross-section point.
    for (let k = 0; k < AROUND; k++) {
      const v = (k / AROUND) * TAU;
      for (let i = 0; i < RINGS; i++) {
        const u0 = start + ((end - start) * i) / RINGS;
        const u1 = start + ((end - start) * (i + 1)) / RINGS;
        push(shell, at(u0, v), at(u1, v));
      }
    }

    // The two severed ends, each a disc facing along the ring.
    const cut: Seg[] = [];
    for (const u of [start, end]) {
      const centre = new THREE.Vector3(MAJOR * Math.cos(u), 0, MAJOR * Math.sin(u));
      const normal = new THREE.Vector3(-Math.sin(u), 0, Math.cos(u));
      cut.push(...disc(centre, normal, MINOR, 6));
    }

    const middle = (start + end) / 2;
    pieces.push({
      shell: Float32Array.from(shell),
      cut: flatten(cut),
      direction: [Math.cos(middle), 0, Math.sin(middle)],
    });
  }

  return { frame: 2.1, spread: 2, pieces };
}

/* -------------------------------------------------------------------------
 * Geodesic — an icosahedral shell whose twenty patches bloom outwards.
 * ---------------------------------------------------------------------- */

function geodesicAssembly(): Assembly {
  /** Rows the triangular patch is divided into. */
  const DIVISIONS = 6;

  const phi = (1 + Math.sqrt(5)) / 2;
  const corners = [
    [-1, phi, 0],
    [1, phi, 0],
    [-1, -phi, 0],
    [1, -phi, 0],
    [0, -1, phi],
    [0, 1, phi],
    [0, -1, -phi],
    [0, 1, -phi],
    [phi, 0, -1],
    [phi, 0, 1],
    [-phi, 0, -1],
    [-phi, 0, 1],
  ].map(([x, y, z]) => new THREE.Vector3(x, y, z).normalize());

  const faces = [
    [0, 11, 5],
    [0, 5, 1],
    [0, 1, 7],
    [0, 7, 10],
    [0, 10, 11],
    [1, 5, 9],
    [5, 11, 4],
    [11, 10, 2],
    [10, 7, 6],
    [7, 1, 8],
    [3, 9, 4],
    [3, 4, 2],
    [3, 2, 6],
    [3, 6, 8],
    [3, 8, 9],
    [4, 9, 5],
    [2, 4, 11],
    [6, 2, 10],
    [8, 6, 7],
    [9, 8, 1],
  ];

  const pieces: Piece[] = faces.map(([ai, bi, ci]) => {
    const a = corners[ai];
    const b = corners[bi];
    const c = corners[ci];

    // Barycentric lattice over the patch, pushed out onto the sphere.
    const grid: THREE.Vector3[][] = [];
    for (let i = 0; i <= DIVISIONS; i++) {
      const row: THREE.Vector3[] = [];
      for (let j = 0; j <= DIVISIONS - i; j++) {
        const k = DIVISIONS - i - j;
        row.push(
          new THREE.Vector3()
            .addScaledVector(a, k / DIVISIONS)
            .addScaledVector(b, i / DIVISIONS)
            .addScaledVector(c, j / DIVISIONS)
            .normalize(),
        );
      }
      grid.push(row);
    }

    // Each lattice edge once, in all three directions of the triangle.
    const shell: number[] = [];
    for (let i = 0; i < DIVISIONS; i++) {
      for (let j = 0; j < DIVISIONS - i; j++) {
        push(shell, grid[i][j], grid[i][j + 1]);
        push(shell, grid[i][j], grid[i + 1][j]);
        push(shell, grid[i][j + 1], grid[i + 1][j]);
      }
    }

    const centroid = new THREE.Vector3().add(a).add(b).add(c).normalize();

    return {
      shell: Float32Array.from(shell),
      // The patch outline, drawn heavier so the pieces read as pieces.
      cut: Float32Array.from([
        a.x,
        a.y,
        a.z,
        b.x,
        b.y,
        b.z,
        b.x,
        b.y,
        b.z,
        c.x,
        c.y,
        c.z,
        c.x,
        c.y,
        c.z,
        a.x,
        a.y,
        a.z,
      ]),
      direction: [centroid.x, centroid.y, centroid.z],
    };
  });

  return { frame: 2.05, spread: 2, pieces };
}

/** Building an assembly is not cheap; do it once per kind per session. */
const cache = new Map<ExplodedKind, Assembly>();

function assemblyFor(kind: ExplodedKind) {
  const cached = cache.get(kind);
  if (cached) return cached;

  const built =
    kind === "sphere" ? sphereAssembly() : kind === "torus" ? torusAssembly() : geodesicAssembly();

  cache.set(kind, built);
  return built;
}

function Assembly({
  kind,
  bloom,
  separation,
  thin,
  color,
  opacity,
  tilt,
  yaw,
  still,
}: {
  kind: ExplodedKind;
  bloom: React.RefObject<number>;
  separation: number;
  thin: number;
  color: string;
  opacity: number;
  tilt: number;
  yaw: number;
  still: boolean;
}) {
  const holder = useRef<THREE.Group>(null);
  const invalidate = useThree((state) => state.invalidate);

  const drawing = useMemo(() => {
    const { pieces, frame } = assemblyFor(kind);
    const shell = new THREE.LineBasicMaterial({ transparent: true });
    const section = new THREE.LineBasicMaterial({ transparent: true });

    const built = pieces.map((piece) => {
      const attach = (positions: Float32Array) => {
        const geometry = new THREE.BufferGeometry();
        geometry.setAttribute("position", new THREE.BufferAttribute(positions, 3));
        return geometry;
      };

      return {
        shell: attach(piece.shell),
        cut: piece.cut.length ? attach(piece.cut) : null,
        direction: new THREE.Vector3(...piece.direction),
        pivot: piece.pivot,
      };
    });

    return { pieces: built, materials: { shell, section }, frame };
  }, [kind]);

  useEffect(() => {
    drawing.materials.shell.color.set(color);
    drawing.materials.shell.opacity = opacity;
    drawing.materials.section.color.set(color);
    // Where the solid was cut carries more weight than the shell it came out
    // of: the section is the point of an exploded drawing.
    drawing.materials.section.opacity = Math.min(1, opacity * 1.9);
    invalidate();
  }, [drawing, color, opacity, invalidate]);

  useEffect(
    () => () => {
      for (const piece of drawing.pieces) {
        piece.shell.dispose();
        piece.cut?.dispose();
      }
      drawing.materials.shell.dispose();
      drawing.materials.section.dispose();
    },
    [drawing],
  );

  // Chased rather than sprung: the target is a scroll position, so it is
  // already smooth, and easing towards it costs one multiply per frame.
  const open = useRef(still ? 1 : 0);

  useFrame(() => {
    const group = holder.current;
    if (!group) return;

    open.current += ((still ? 1 : bloom.current) - open.current) * (still ? 1 : 0.12);

    // How much of its width a slice still has. Fat when the object is off at
    // the edge of the screen, thin as it crosses the middle.
    const width = 1 - open.current * thin;

    for (let i = 0; i < group.children.length; i++) {
      const piece = drawing.pieces[i];
      const child = group.children[i];

      if (piece.pivot === undefined) {
        child.position.copy(piece.direction).multiplyScalar(open.current * separation);
        continue;
      }

      // Scaling runs from the object's own origin, so the slice is pushed back
      // out to sit on its middle again.
      child.scale.x = width;
      child.position.x = piece.pivot * (1 - width);
    }

    // The solid is held at one attitude. The only thing the scroll moves is
    // how far the sections have drawn off the body.
    group.rotation.set(tilt, yaw, 0);
  });

  return (
    <group ref={holder}>
      {drawing.pieces.map((piece, index) => (
        // biome-ignore lint/suspicious/noArrayIndexKey: `drawing.pieces` is derived geometry of fixed length and order; pieces are never added, removed, or sorted
        <group key={index}>
          <lineSegments geometry={piece.shell} material={drawing.materials.shell} />
          {piece.cut ? (
            <lineSegments geometry={piece.cut} material={drawing.materials.section} />
          ) : null}
        </group>
      ))}
    </group>
  );
}

/** Keeps the assembly framed, whatever the canvas ends up being. */
function Frame({ size: world }: { size: number }) {
  const camera = useThree((state) => state.camera);
  const size = useThree((state) => state.size);

  useEffect(() => {
    const orthographic = camera as THREE.OrthographicCamera;
    orthographic.zoom = Math.min(size.width, size.height) / world;
    orthographic.updateProjectionMatrix();
  }, [camera, size, world]);

  return null;
}

export function ExplodedObject({
  kind,
  separation = 0.32,
  thin = 0.62,
  tilt = 0.26,
  yaw = -0.24,
  // The shells are drawn dense, so each line carries little on its own: the
  // weight comes from how many of them cross.
  opacity = 0.12,
  className,
}: {
  kind: ExplodedKind;
  /** How far a travelling piece goes at the centre of the screen. */
  separation?: number;
  /** How much of its width a slice gives up at the centre of the screen. */
  thin?: number;
  /** X rotation. The solid is held at it — nothing here turns. */
  tilt?: number;
  /** Y rotation, so the cut faces are seen at an angle rather than edge-on. */
  yaw?: number;
  /** Stroke alpha for the shells; sections are drawn heavier. */
  opacity?: number;
  className?: string;
}) {
  const host = useRef<HTMLDivElement>(null);
  const { color, onLight } = useInkColor(host);
  const bloom = useViewportBloom(host);
  const [inView, setInView] = useState(false);
  const [still, setStill] = useState(false);

  useEffect(() => {
    const query = window.matchMedia("(prefers-reduced-motion: reduce)");
    const sync = () => setStill(query.matches);
    sync();
    query.addEventListener("change", sync);
    return () => query.removeEventListener("change", sync);
  }, []);

  useEffect(() => {
    const element = host.current;
    if (!element) return;

    const observer = new IntersectionObserver(([entry]) => setInView(entry.isIntersecting), {
      rootMargin: "150px 0px",
    });
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  const { frame, spread } = assemblyFor(kind);

  return (
    <div ref={host} className={cn("text-foreground", className)}>
      <Canvas
        orthographic
        frameloop={inView && !still ? "always" : "demand"}
        // The ink is a design token; tone mapping would repaint it.
        flat
        dpr={[1, 2]}
        gl={{ antialias: true, alpha: true }}
        camera={{ position: [0, 0, 10], near: -50, far: 50, zoom: 60 }}
      >
        {/* Framed at the width it reaches drawn apart, so it never crops. */}
        <Frame size={frame + separation * spread} />
        <Assembly
          kind={kind}
          bloom={bloom}
          separation={separation}
          thin={thin}
          color={color}
          opacity={opacity ? inkOpacity(opacity, onLight) : 0}
          tilt={tilt}
          yaw={yaw}
          still={still}
        />
      </Canvas>
    </div>
  );
}
