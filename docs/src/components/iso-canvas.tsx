"use client";

import {
  DatabaseIcon,
  GitBranchIcon,
  PlugsConnectedIcon,
  RobotIcon,
} from "@phosphor-icons/react/dist/ssr";
import { Html, OrbitControls } from "@react-three/drei";
import { Canvas, useThree } from "@react-three/fiber";
import { type RefObject, useEffect, useMemo, useRef, useState } from "react";
import * as THREE from "three";
import type { OrbitControls as OrbitControlsImpl } from "three-stdlib";

import { Logo } from "@/components/logo";
import { inkOpacity, useInkColor, useSurfaceColor } from "@/hooks/use-ink-color";
import type { MarkName } from "@/lib/iso-marks";
import type { Ground, IsoConnector, IsoMetrics, IsoPlate, IsoScene } from "@/lib/iso-scene";
import { frameSpan } from "@/lib/iso-scene";
import { cn } from "@/lib/utils";

/**
 * The isometric renderer: real geometry under an orthographic camera on the
 * isometric diagonal, exactly as `iso-stack.tsx` does it, with the captions
 * laid *into* the same plane as live DOM.
 *
 * That last part is the whole trick. A label here is not text drawn onto the
 * scene, and it is not text floating upright in front of it — it is a `<div>`
 * parented to a group rotated into the ground plane, so three.js carries it
 * through the same projection as the slab it sits on. It comes out lying on
 * the plate like ink on a drawing board, and because it is still live DOM it
 * keeps the page's own font, its theme tokens and its subpixel rendering. The
 * marks ride in the same `<div>`, so the rift logo and the icons land in the
 * projection too rather than hovering flat over it.
 *
 * This is the opposite of skewing a flat SVG: nothing is rasterised and then
 * distorted, the browser composites the text under a real 3D transform.
 */

/**
 * Ties CSS pixels to world units: drei's `Html transform` scales its content by
 * `distanceFactor / 400`, so a design authored in pixels lands at a known size
 * in the scene only if the geometry uses the same conversion.
 */
const DISTANCE_FACTOR = 8;
const PX = DISTANCE_FACTOR / 400;

/** Clear of the plate's top face, so the caption never z-fights the slab. */
const EPS = 0.006;

/** Far enough back that no plate escapes the frustum; orthographic, so free. */
const CAMERA_DISTANCE = 600;

/**
 * Ink weights. A slab is read from its drawn edges rather than any fill, per
 * the design system — the solid inside it is the page's own background, and it
 * is there only to occlude.
 */
const INK = {
  plate: 0.5,
  connector: 0.95,
  floor: 0.14,
};

/** How far the floor runs past the drawing, as a multiple of its own size. */
const REACH = 1.4;

/** Every Nth rule on the floor is drawn heavier. */
const MAJOR = 5;

/** What a mark actually looks like. Keyed by the names in `iso-marks.ts`. */
const MARKS: Record<MarkName, (size: number) => React.ReactNode> = {
  // The mesh silts up into a blob below about 24px, so this placement takes
  // the sparse variant with a much heavier stroke.
  rift: (size) => (
    <Logo
      stitches={6}
      strokeWidth={3.2}
      stringOpacity={0.6}
      className="shrink-0"
      style={{ width: size, height: size }}
    />
  ),
  agent: (size) => <RobotIcon size={size} weight="light" className="shrink-0" />,
  store: (size) => <DatabaseIcon size={size} weight="light" className="shrink-0" />,
  git: (size) => <GitBranchIcon size={size} weight="light" className="shrink-0" />,
  adapter: (size) => <PlugsConnectedIcon size={size} weight="light" className="shrink-0" />,
};

/** A ground-plane point, lifted to a height and scaled into world units. */
function at([x, z]: Ground, y: number): [number, number, number] {
  return [x * PX, y, z * PX];
}

/**
 * A caption lying in the ground plane. The rotation is what puts it there: a
 * quarter turn about X lays the div flat, with its own reading direction along
 * the flow axis, so text runs with the diagram rather than across it.
 */
function Caption({
  position,
  children,
}: {
  position: [number, number, number];
  children: React.ReactNode;
}) {
  return (
    <group position={position} rotation={[-Math.PI / 2, 0, 0]}>
      <Html
        transform
        center
        distanceFactor={DISTANCE_FACTOR}
        // Kept out of the page's own stacking order — a label belongs to the
        // drawing, not on top of the nav.
        zIndexRange={[0, 0]}
        className="pointer-events-none select-none"
      >
        {children}
      </Html>
    </group>
  );
}

function Plate({
  plate,
  metrics,
  ink,
  surface,
  alpha,
}: {
  plate: IsoPlate;
  metrics: IsoMetrics;
  ink: string;
  surface: string;
  alpha: number;
}) {
  const thickness = metrics.thickness * PX;

  // A flat plate is a plane, not a box of zero height. A degenerate box gives
  // `EdgesGeometry` two coplanar rectangles to find, which draws every edge
  // twice at the same depth and z-fights.
  const geometry = useMemo(() => {
    if (thickness <= 0) {
      const plane = new THREE.PlaneGeometry(plate.width * PX, plate.depth * PX);
      plane.rotateX(-Math.PI / 2); // PlaneGeometry stands up in XY; lay it on the floor
      return { box: plane, edges: new THREE.EdgesGeometry(plane) };
    }
    const box = new THREE.BoxGeometry(plate.width * PX, thickness, plate.depth * PX);
    return { box, edges: new THREE.EdgesGeometry(box) };
  }, [plate.width, plate.depth, thickness]);

  useEffect(
    () => () => {
      geometry.box.dispose();
      geometry.edges.dispose();
    },
    [geometry],
  );

  const mark = plate.mark ? MARKS[plate.mark as MarkName] : null;

  return (
    <group position={[plate.x * PX, 0, plate.z * PX]}>
      {/* A solid in the page's own colour: the plate occludes what is behind
          it without ever reading as a filled shape. */}
      {/* A box sits on the floor, so its centre is half its height. A flat
          plate has no height to halve, and would land in the floor's own
          plane — so it takes half the clearance the connectors use, which
          stacks it above the grid and below them. */}
      <mesh geometry={geometry.box} position={[0, thickness > 0 ? thickness / 2 : EPS / 2, 0]}>
        <meshBasicMaterial color={surface} />
      </mesh>
      <lineSegments
        geometry={geometry.edges}
        position={[0, thickness > 0 ? thickness / 2 : EPS / 2, 0]}
      >
        <lineBasicMaterial color={ink} transparent opacity={alpha} />
      </lineSegments>

      <Caption position={[0, thickness + EPS, 0]}>
        <div
          className="flex items-center whitespace-nowrap font-mono leading-tight text-foreground"
          style={{ fontSize: metrics.label, gap: metrics.gutter }}
        >
          {mark ? mark(metrics.icon) : null}
          <span className="grid">
            {plate.label.map((line) => (
              <span key={line}>{line}</span>
            ))}
          </span>
        </div>
      </Caption>
    </group>
  );
}

function Connector({
  connector,
  metrics,
  ink,
  alpha,
}: {
  connector: IsoConnector;
  metrics: IsoMetrics;
  ink: string;
  alpha: number;
}) {
  // Connectors run on the floor, not on the plate tops: a link is something
  // crossing the ground between two slabs, so it belongs in the plane the
  // slabs stand on. `EPS` is only enough clearance to keep it off the grid it
  // now shares that plane with.

  const run = useMemo(() => {
    // Expanded into pairs for `lineSegments` rather than drawn as one strip.
    // A hairline has no joins to lose, and it keeps the connectors on exactly
    // the same material — and so exactly the same width — as the plate edges,
    // which a screen-space line would not.
    const points: number[] = [];
    for (let i = 0; i < connector.points.length - 1; i++) {
      points.push(...at(connector.points[i], EPS), ...at(connector.points[i + 1], EPS));
    }
    const geometry = new THREE.BufferGeometry();
    geometry.setAttribute("position", new THREE.Float32BufferAttribute(points, 3));
    return geometry;
  }, [connector.points]);

  // A dash pattern needs distances along the run, and only the drawn object
  // can work them out — the geometry alone does not know it is a line.
  const dashed = useRef<THREE.LineSegments>(null);
  useEffect(() => {
    if (connector.style === "dotted") dashed.current?.computeLineDistances();
  }, [connector.style]);

  const head = useMemo(() => {
    if (!connector.head) return null;
    const geometry = new THREE.BufferGeometry();
    geometry.setAttribute(
      "position",
      new THREE.Float32BufferAttribute(
        connector.head.flatMap((point) => at(point, EPS)),
        3,
      ),
    );
    return geometry;
  }, [connector.head]);

  useEffect(
    () => () => {
      run.dispose();
      head?.dispose();
    },
    [run, head],
  );

  return (
    <>
      <lineSegments ref={dashed} geometry={run}>
        {connector.style === "dotted" ? (
          <lineDashedMaterial
            color={ink}
            transparent
            opacity={alpha}
            dashSize={0.05}
            gapSize={0.04}
          />
        ) : (
          <lineBasicMaterial color={ink} transparent opacity={alpha} />
        )}
      </lineSegments>
      {head ? (
        <mesh geometry={head}>
          <meshBasicMaterial color={ink} transparent opacity={alpha} side={THREE.DoubleSide} />
        </mesh>
      ) : null}

      {connector.labelAt ? (
        <Caption position={at(connector.labelAt, EPS * 2)}>
          {/* Knocked out against the page, so a label wins over the connector
              it names. `bg-background` is the token, so it inverts with it. */}
          <div
            className="whitespace-nowrap bg-background text-center font-mono leading-tight text-muted-foreground"
            style={{ fontSize: metrics.note, padding: `${metrics.note * 0.3}px ${metrics.note}px` }}
          >
            {connector.label.map((line) => (
              <div key={line}>{line}</div>
            ))}
          </div>
        </Caption>
      ) : null}
    </>
  );
}

/**
 * The ground the plates stand on: graph paper, ruled on both axes, carrying on
 * well past the drawing.
 *
 * It reaches `REACH` times the scene's own size in every direction rather than
 * stopping at the plates. The diagram is the thing standing *on* a surface, so
 * the surface has to be plainly bigger than it — and now that the scene can be
 * panned and zoomed, a floor cut to the outline of the plates would end in
 * mid-air the moment anyone moved.
 *
 * Drawn as real line geometry rather than a grid shader so the rules take the
 * same hairline material as everything else in the drawing, and so their
 * weight is an opacity on the design's own ink instead of a colour mixed by
 * hand towards the page.
 */
function Floor({ scene, ink, alpha }: { scene: IsoScene; ink: string; alpha: number }) {
  const { floor } = scene.metrics;

  const geometry = useMemo(() => {
    if (floor <= 0) return null;

    const { bounds } = scene;
    const reach = Math.max(bounds.x1 - bounds.x0, bounds.z1 - bounds.z0) * REACH;
    const field = {
      x0: bounds.x0 - reach,
      x1: bounds.x1 + reach,
      z0: bounds.z0 - reach,
      z1: bounds.z1 + reach,
    };

    // Both families come off one lattice through the origin, so they cross at
    // shared points however the drawing happens to sit on it.
    const first = (at: number) => Math.ceil(at / floor) * floor;
    const minor: number[] = [];
    const major: number[] = [];

    for (let x = first(field.x0); x <= field.x1; x += floor) {
      const into = Math.round(x / floor) % MAJOR === 0 ? major : minor;
      into.push(...at([x, field.z0], 0), ...at([x, field.z1], 0));
    }
    for (let z = first(field.z0); z <= field.z1; z += floor) {
      const into = Math.round(z / floor) % MAJOR === 0 ? major : minor;
      into.push(...at([field.x0, z], 0), ...at([field.x1, z], 0));
    }

    const build = (points: number[]) => {
      const buffer = new THREE.BufferGeometry();
      buffer.setAttribute("position", new THREE.Float32BufferAttribute(points, 3));
      return buffer;
    };

    return { minor: build(minor), major: build(major) };
  }, [scene, floor]);

  useEffect(
    () => () => {
      geometry?.minor.dispose();
      geometry?.major.dispose();
    },
    [geometry],
  );

  if (!geometry) return null;

  return (
    <>
      <lineSegments geometry={geometry.minor}>
        <lineBasicMaterial color={ink} transparent opacity={alpha} />
      </lineSegments>
      {/* Every fifth rule carries more weight, so the plane still reads as a
          plane once it is pushed away instead of going to a flat wash. */}
      <lineSegments geometry={geometry.major}>
        <lineBasicMaterial color={ink} transparent opacity={alpha * 2.1} />
      </lineSegments>
    </>
  );
}

/**
 * Sits the camera on the isometric diagonal and zooms it until the drawing
 * fills the canvas. Orthographic, so distance is free and only zoom is seen.
 *
 * It frames once and then lets go: past that the controls own the camera, and
 * re-framing under someone who has just zoomed in would fight them.
 */
function Frame({
  scene,
  controls,
}: {
  scene: IsoScene;
  controls: RefObject<OrbitControlsImpl | null>;
}) {
  const camera = useThree((state) => state.camera);
  const size = useThree((state) => state.size);
  const invalidate = useThree((state) => state.invalidate);
  const framed = useRef(false);

  useEffect(() => {
    if (framed.current) return;
    framed.current = true;

    const { bounds, metrics } = scene;
    const centre = new THREE.Vector3(
      ((bounds.x0 + bounds.x1) / 2) * PX,
      (metrics.thickness * PX) / 2,
      ((bounds.z0 + bounds.z1) / 2) * PX,
    );

    const span = frameSpan(scene);
    const orthographic = camera as THREE.OrthographicCamera;

    orthographic.position.set(
      centre.x + CAMERA_DISTANCE,
      centre.y + CAMERA_DISTANCE,
      centre.z + CAMERA_DISTANCE,
    );
    orthographic.up.set(0, 1, 0);
    orthographic.lookAt(centre);
    // r3f's orthographic frustum is the canvas in pixels, so zoom *is* pixels
    // per world unit — fitting is one division.
    orthographic.zoom = Math.min(size.width / (span.width * PX), size.height / (span.height * PX));
    orthographic.updateProjectionMatrix();

    controls.current?.target.copy(centre);
    controls.current?.update();
    invalidate();
  }, [scene, camera, size, invalidate, controls]);

  return null;
}

export function IsoCanvas({
  scene,
  alt,
  className,
}: {
  scene: IsoScene;
  alt: string;
  className?: string;
}) {
  const host = useRef<HTMLDivElement>(null);
  const controls = useRef<OrbitControlsImpl>(null);
  const { color, onLight } = useInkColor(host);
  const surface = useSurfaceColor();
  // Starts on, and the observer only ever turns it *off*. drei's `Html` places
  // itself from a frame, so a figure that began parked would hold its captions
  // unpositioned until something happened to ask for one.
  const [inView, setInView] = useState(true);

  // The scene is driveable now, and drei's `Html` re-measures on a frame, so
  // the loop runs while the figure is on screen and stops the moment it is not.
  useEffect(() => {
    const element = host.current;
    if (!element) return;

    const observer = new IntersectionObserver(([entry]) => setInView(entry.isIntersecting), {
      rootMargin: "200px 0px",
    });
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  const span = frameSpan(scene);

  return (
    <div
      ref={host}
      role="img"
      aria-label={alt}
      className={cn("w-full text-foreground", className)}
      style={{ aspectRatio: `${span.width} / ${span.height}` }}
    >
      <Canvas
        orthographic
        frameloop={inView ? "always" : "demand"}
        // No tone mapping: the ink and the occluder are design tokens, and
        // either one repainted is a grey slab on a light page.
        flat
        dpr={[1, 2]}
        gl={{ antialias: true, alpha: true }}
        camera={{ position: [1, 1, 1], near: 0.1, far: CAMERA_DISTANCE * 4, zoom: 1 }}
      >
        <Frame scene={scene} controls={controls} />
        {/*
          Driveable, but not free. The captions lie *in* the ground plane, so
          they are only legible from above it and from roughly the quadrant the
          scene was composed for — swing past that and the text goes edge-on or
          mirrors. The limits keep the camera inside the range where the
          drawing still reads, and leave zoom and pan unbounded, which is what
          someone reading a dense diagram actually wants.
        */}
        <OrbitControls
          ref={controls}
          makeDefault
          enableDamping
          dampingFactor={0.12}
          minPolarAngle={Math.PI * 0.12}
          maxPolarAngle={Math.PI * 0.46}
          minAzimuthAngle={0}
          maxAzimuthAngle={Math.PI / 2}
          minZoom={6}
          maxZoom={600}
          zoomSpeed={0.7}
          screenSpacePanning
        />
        <Floor scene={scene} ink={color} alpha={inkOpacity(INK.floor, onLight)} />
        {scene.plates.map((plate) => (
          <Plate
            key={plate.id}
            plate={plate}
            metrics={scene.metrics}
            ink={color}
            surface={surface}
            alpha={inkOpacity(INK.plate, onLight)}
          />
        ))}
        {scene.connectors.map((connector) => (
          <Connector
            key={connector.key}
            connector={connector}
            metrics={scene.metrics}
            ink={color}
            alpha={inkOpacity(INK.connector, onLight)}
          />
        ))}
      </Canvas>
    </div>
  );
}
