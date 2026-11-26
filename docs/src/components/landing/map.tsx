"use client";

import { useReducedMotion } from "@react-spring/web";
import { Canvas, useFrame, useThree } from "@react-three/fiber";
import { useTheme } from "next-themes";
import { type RefObject, useEffect, useMemo, useRef, useState } from "react";
import * as THREE from "three";
import { buildSphereIntake } from "./sphere";
import {
  buildRoad,
  buildTerrain,
  type RoadStop,
  roadElevation,
  terrainShaders,
  viewHeight,
} from "./terrain";
import { buildTracers, strandShaders, traceShaders } from "./tracers";

const UP = new THREE.Vector3(0, 1, 0);
const ISOMETRIC_YAW = Math.PI / 4;
const ISOMETRIC_LIFT = Math.sqrt(2 / 3);

const DARK = { paper: "#0c0d10", ink: "#9f9ba9", ruby: "#f04f38", stars: "#f2d8e3" };
const LIGHT = { paper: "#f8f8fa", ink: "#4e4147", ruby: "#a51f15", stars: "#6e485e" };

type SphereSlot = { across: number; down: number; radius: number };
type MapLayout = { compact: boolean; stops: RoadStop[]; sphereSlot: SphereSlot };

function scrollBounds() {
  const root = document.documentElement;
  return {
    start: Number.parseFloat(getComputedStyle(root).scrollPaddingTop) || 0,
    end: Math.max(0, root.scrollHeight - root.clientHeight),
  };
}

function useRoute(host: RefObject<HTMLDivElement | null>, reduced: boolean) {
  const invalidate = useThree((state) => state.invalidate);
  const target = useRef(0);
  const progress = useRef(0);

  useEffect(() => {
    const page = host.current?.parentElement;
    if (!page) return;
    const sections = [...page.querySelectorAll<HTMLElement>("[data-map-stop]")];
    if (sections.length < 2) return;
    let stops: number[] = [];
    let frame = 0;

    const sample = () => {
      frame = 0;
      const scroll = window.scrollY;
      const next = stops.findIndex((stop) => stop > scroll);
      const destination =
        next === -1
          ? stops.length - 1
          : next === 0
            ? 0
            : next - 1 + (scroll - stops[next - 1]) / (stops[next] - stops[next - 1]);
      target.current = reduced ? Math.round(destination) : destination;
      invalidate();
    };

    const measure = () => {
      const scroll = window.scrollY;
      const { start, end } = scrollBounds();
      stops = sections.map((section) =>
        THREE.MathUtils.clamp(scroll + section.getBoundingClientRect().top - start, 0, end),
      );
      sample();
    };

    const onScroll = () => {
      if (!frame) frame = requestAnimationFrame(sample);
    };

    measure();
    progress.current = target.current;
    const observer = new ResizeObserver(measure);
    observer.observe(page);
    window.addEventListener("resize", measure);
    window.addEventListener("scroll", onScroll, { passive: true });

    return () => {
      observer.disconnect();
      window.removeEventListener("resize", measure);
      window.removeEventListener("scroll", onScroll);
      if (frame) cancelAnimationFrame(frame);
    };
  }, [host, invalidate, reduced]);

  return { target, progress };
}

function useRoadStops(host: RefObject<HTMLDivElement | null>, enabled: boolean) {
  const [layout, setLayout] = useState<MapLayout>();
  useEffect(() => {
    const element = host.current;
    if (!enabled || !element?.parentElement) return;
    const page = element.parentElement;
    const sphere = page.querySelector<HTMLElement>(".landing-stop__sphere");
    if (!sphere?.parentElement) return;
    const finale = sphere.parentElement;
    const panels = [...page.querySelectorAll<HTMLElement>(".landing-stop__panel")].flatMap(
      (panel) => {
        const section = panel.closest("[data-map-stop]");
        return section ? [{ panel, section }] : [];
      },
    );
    let frame = 0;

    const measure = () => {
      frame = 0;
      const viewport = element.getBoundingClientRect();
      const sphereBox = sphere.getBoundingClientRect();
      if (
        !viewport.width ||
        !viewport.height ||
        !sphereBox.width ||
        !sphereBox.height ||
        panels.length < 2
      )
        return;
      const compact = viewport.width < 768;
      const units = viewHeight(viewport.width) / viewport.height;
      const center = viewport.left + viewport.width / 2;
      const { start, end } = scrollBounds();
      const scroll = window.scrollY;
      const finalSnap = THREE.MathUtils.clamp(
        scroll + finale.getBoundingClientRect().top - start,
        0,
        end,
      );
      const sphereSlot = {
        across: (sphereBox.left + sphereBox.width / 2 - center) * units,
        down:
          (scroll +
            sphereBox.top +
            sphereBox.height / 2 -
            finalSnap -
            viewport.top -
            viewport.height / 2) *
          units,
        radius: (Math.min(sphereBox.width, sphereBox.height) * units) / 2,
      };
      const stops = panels.map(({ panel, section }) => {
        const box = panel.getBoundingClientRect();
        const sectionTop = section.getBoundingClientRect().top;
        const snapped = THREE.MathUtils.clamp(scroll + sectionTop - start, 0, end);
        const hero = section.id === "overview";
        const left = box.left + box.width / 2 < center;
        const edge = left ? box.right - 3 : box.left + 3;
        const anchor = hero ? box.bottom - 3 : compact ? box.top + 2 : box.top + box.height * 0.6;
        const vertical = scroll + anchor - snapped - viewport.top - viewport.height / 2;
        return {
          across: compact || hero ? 0 : (edge - center) * units,
          lift: (vertical * units) / ISOMETRIC_LIFT,
        };
      });

      setLayout((previous) =>
        previous?.compact === compact &&
        Math.abs(previous.sphereSlot.across - sphereSlot.across) < 0.012 &&
        Math.abs(previous.sphereSlot.down - sphereSlot.down) < 0.012 &&
        Math.abs(previous.sphereSlot.radius - sphereSlot.radius) < 0.012 &&
        stops.every((stop, index) => {
          const old = previous.stops[index];
          return (
            old &&
            Math.abs(old.across - stop.across) < 0.012 &&
            Math.abs(old.lift - stop.lift) < 0.012
          );
        })
          ? previous
          : { compact, stops, sphereSlot },
      );
    };

    const observer = new ResizeObserver(() => {
      if (!frame) frame = requestAnimationFrame(measure);
    });
    observer.observe(element);
    observer.observe(sphere);
    for (const { panel } of panels) observer.observe(panel);
    measure();
    return () => {
      observer.disconnect();
      if (frame) cancelAnimationFrame(frame);
    };
  }, [enabled, host]);
  return layout;
}

function MapScene({
  host,
  reduced,
  light,
  compact,
  stops,
  sphereSlot,
}: {
  host: RefObject<HTMLDivElement | null>;
  reduced: boolean;
  light: boolean;
} & MapLayout) {
  const { progress, target } = useRoute(host, reduced);
  const palette = light ? LIGHT : DARK;
  const lastStop = stops.length - 1;
  const road = useMemo(() => buildRoad(stops, compact), [compact, stops]);
  const terrain = useMemo(() => buildTerrain(lastStop, road), [lastStop, road]);
  const { width, height } = useThree((state) => state.size);
  const sphere = useMemo(() => {
    const { across: x, down, radius } = sphereSlot;
    const { y: along } = road(lastStop, new THREE.Vector2());
    const cameraY = roadElevation(lastStop) + stops[lastStop].lift;
    let y = cameraY - down / ISOMETRIC_LIFT;
    let z = along;
    // Move toward the isometric camera to clear the terrain while keeping
    // the projection centered in the reserved layout slot.
    for (let pass = 0; pass < 5; pass++) {
      const clearance = terrain.height(x, z) + radius * 1.55 - y;
      if (clearance <= 0.01) break;
      y += clearance;
      z += clearance * Math.SQRT2;
    }
    return { center: new THREE.Vector3(x, y, z), radius };
  }, [lastStop, road, sphereSlot, stops, terrain]);
  const intake = useMemo(() => {
    const view = viewHeight(width);
    const projected = new THREE.Vector2((view * width) / Math.max(1, height), view);
    return buildSphereIntake(sphere.center, sphere.radius, compact, projected, sphereSlot.down);
  }, [sphere, compact, width, height, sphereSlot.down]);
  const tracers = useMemo(
    () => buildTracers(lastStop, road, terrain.height, intake),
    [lastStop, road, terrain, intake],
  );
  const routeFlow = useRef<THREE.InstancedMesh>(null);
  const sphereFlow = useRef<THREE.InstancedMesh>(null);
  const phase = useRef(0);
  const visible = useRef(true);
  const uniforms = useMemo(
    () => ({
      focus: { value: new THREE.Vector2() },
      paper: { value: new THREE.Color(palette.paper) },
      ink: { value: new THREE.Color(palette.ink) },
      ruby: { value: new THREE.Color(palette.ruby) },
      stars: { value: new THREE.Color(palette.stars) },
      traceRoute: { value: tracers.route },
      traceSize: { value: tracers.size },
      animation: { value: new THREE.Vector2() },
      sphereCenter: { value: sphere.center },
      sphereRadius: { value: sphere.radius },
      intakePaths: { value: intake.paths },
      intakeSize: { value: intake.size },
      approachSamples: { value: intake.approachSamples },
      feedLength: { value: intake.feedLength },
      routeLength: { value: tracers.routeLength },
      baseTraceCount: { value: tracers.count },
      lightMode: { value: Number(light) },
    }),
    [light, palette, sphere, tracers, intake],
  );

  const lookAt = useMemo(() => new THREE.Vector3(), []);
  const course = useMemo(() => new THREE.Vector2(), []);
  const invalidate = useThree((state) => state.invalidate);

  useEffect(() => {
    const syncVisibility = () => {
      visible.current = document.visibilityState === "visible";
      if (visible.current && !reduced) invalidate();
    };
    syncVisibility();
    document.addEventListener("visibilitychange", syncVisibility);
    return () => document.removeEventListener("visibilitychange", syncVisibility);
  }, [invalidate, reduced]);

  useEffect(() => () => terrain.geometry.dispose(), [terrain]);
  useEffect(
    () => () => {
      tracers.geometry.dispose();
      tracers.strands.dispose();
      tracers.route.dispose();
      intake.paths.dispose();
    },
    [tracers, intake],
  );

  useFrame(({ camera, size, invalidate }, delta) => {
    const flowing = !reduced && visible.current;
    if (flowing) {
      phase.current = (phase.current + Math.min(delta, 0.06) * tracers.speed) % 1;
    }
    const distance = Math.abs(progress.current - target.current);
    progress.current =
      distance < 0.002 || reduced
        ? target.current
        : THREE.MathUtils.damp(progress.current, target.current, 8, Math.min(delta, 0.06));
    const position = progress.current;
    const gathering = THREE.MathUtils.smoothstep(position, lastStop - 0.82, lastStop - 0.04);
    uniforms.animation.value.set(phase.current, gathering);
    const count = gathering > 0.34 ? tracers.finaleCount : tracers.count;
    if (routeFlow.current) routeFlow.current.count = count;
    if (sphereFlow.current) {
      sphereFlow.current.count = count;
      sphereFlow.current.visible = gathering > 0.08;
    }
    if (flowing || progress.current !== target.current) invalidate();

    const height = viewHeight(size.width);
    const { x: across, y: along } = road(position, course);
    const orthographic = camera as THREE.OrthographicCamera;
    const zoom = size.height / height;
    if (orthographic.zoom !== zoom) {
      orthographic.zoom = zoom;
      orthographic.updateProjectionMatrix();
    }

    const section = Math.floor(position);
    const near = stops[section];
    const far = stops[Math.min(lastStop, section + 1)];
    const cameraLift = THREE.MathUtils.lerp(near.lift, far.lift, position - section);
    // Travel uphill with the road while keeping its waypoints under the HTML panels.
    lookAt.set(0, roadElevation(position) + cameraLift, along).applyAxisAngle(UP, ISOMETRIC_YAW);
    camera.position.set(lookAt.x + 42, lookAt.y + 42, lookAt.z + 42);
    camera.lookAt(lookAt);
    uniforms.focus.value.set(across, along - 3);
  });

  return (
    <group rotation={[0, ISOMETRIC_YAW, 0]}>
      <mesh geometry={terrain.geometry}>
        <shaderMaterial {...terrainShaders} uniforms={uniforms} toneMapped={false} />
      </mesh>
      <instancedMesh
        ref={routeFlow}
        args={[undefined, undefined, tracers.finaleCount]}
        geometry={tracers.geometry}
        frustumCulled={false}
      >
        <shaderMaterial
          {...traceShaders}
          uniforms={uniforms}
          side={THREE.DoubleSide}
          transparent
          depthWrite={false}
          toneMapped={false}
        />
      </instancedMesh>
      <instancedMesh
        ref={sphereFlow}
        args={[undefined, undefined, tracers.finaleCount]}
        geometry={tracers.strands}
        frustumCulled={false}
        renderOrder={1}
      >
        <shaderMaterial
          {...strandShaders}
          uniforms={uniforms}
          side={THREE.DoubleSide}
          transparent
          depthTest={false}
          depthWrite={false}
          toneMapped={false}
        />
      </instancedMesh>
    </group>
  );
}

export function LandingMap() {
  const host = useRef<HTMLDivElement>(null);
  const reduced = useReducedMotion() === true;
  const { resolvedTheme } = useTheme();
  const [webgl, setWebgl] = useState(false);
  const layout = useRoadStops(host, webgl);

  useEffect(() => {
    const context = document.createElement("canvas").getContext("webgl2");
    if (!context) return;
    context.getExtension("WEBGL_lose_context")?.loseContext();
    setWebgl(true);
  }, []);

  return (
    <div ref={host} className="landing-map" aria-hidden="true">
      {layout && (
        <Canvas
          orthographic
          camera={{ position: [42, 42, 42], near: 0.1, far: 300 }}
          frameloop="demand"
          dpr={[1, 1.75]}
          gl={{ antialias: true, alpha: true, powerPreference: "low-power" }}
        >
          <MapScene host={host} reduced={reduced} light={resolvedTheme === "light"} {...layout} />
        </Canvas>
      )}
    </div>
  );
}
