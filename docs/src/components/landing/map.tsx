"use client";

import { useReducedMotion } from "@react-spring/web";
import { Canvas, useFrame, useThree } from "@react-three/fiber";
import { useTheme } from "next-themes";
import { type RefObject, useEffect, useMemo, useRef, useState } from "react";
import * as THREE from "three";
import {
  buildRoad,
  buildTerrain,
  type RoadStop,
  roadElevation,
  terrainShaders,
  viewHeight,
} from "./terrain";
import { buildTracers, traceShaders } from "./tracers";

const UP = new THREE.Vector3(0, 1, 0);
const ISOMETRIC_YAW = Math.PI / 4;
const ISOMETRIC_LIFT = Math.sqrt(2 / 3);

const DARK = { paper: "#0c0d10", ink: "#9f9ba9", ruby: "#f04f38", stars: "#f2d8e3" };
const LIGHT = { paper: "#f8f8fa", ink: "#4e4147", ruby: "#a51f15", stars: "#6e485e" };

function useRoute(host: RefObject<HTMLDivElement | null>, reduced: boolean) {
  const invalidate = useThree((state) => state.invalidate);
  const target = useRef(0);
  const progress = useRef(0);

  useEffect(() => {
    const page = host.current?.parentElement;
    if (!page) return;
    if (reduced) {
      progress.current = 0;
      target.current = 0;
      invalidate();
      return;
    }

    const sections = [...page.querySelectorAll<HTMLElement>("[data-map-stop]")];
    if (sections.length < 2) return;
    let centers: number[] = [];
    let frame = 0;

    const sample = () => {
      frame = 0;
      const eye = window.scrollY + window.innerHeight * 0.5;
      const next = centers.findIndex((center) => center > eye);
      target.current =
        next === -1
          ? centers.length - 1
          : next === 0
            ? 0
            : next - 1 + (eye - centers[next - 1]) / (centers[next] - centers[next - 1]);
      invalidate();
    };

    const measure = () => {
      const scroll = window.scrollY;
      centers = sections.map((section) => {
        const rect = section.getBoundingClientRect();
        return scroll + rect.top + rect.height / 2;
      });
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
  const [layout, setLayout] = useState<{ compact: boolean; stops: RoadStop[] }>();
  useEffect(() => {
    const element = host.current;
    if (!enabled || !element?.parentElement) return;
    const panels = [
      ...element.parentElement.querySelectorAll<HTMLElement>(".landing-stop__panel"),
    ].flatMap((panel) => {
      const section = panel.closest("[data-map-stop]");
      return section ? [{ panel, section }] : [];
    });
    let frame = 0;

    const measure = () => {
      frame = 0;
      const viewport = element.getBoundingClientRect();
      if (!viewport.width || !viewport.height || panels.length < 2) return;
      const compact = viewport.width < 768;
      const units = viewHeight(viewport.width) / viewport.height;
      const center = viewport.left + viewport.width / 2;
      const stops = panels.map(({ panel, section }) => {
        const box = panel.getBoundingClientRect();
        const stop = section.getBoundingClientRect();
        const left = box.left + box.width / 2 < center;
        const edge = left ? box.right - 3 : box.left + 3;
        const anchor = compact ? box.top + 2 : box.top + box.height * 0.6;
        return {
          across: compact ? 0 : (edge - center) * units,
          lift: ((anchor - stop.top - stop.height / 2) * units) / ISOMETRIC_LIFT,
        };
      });

      setLayout((previous) =>
        previous?.compact === compact &&
        stops.every((stop, index) => {
          const old = previous.stops[index];
          return (
            old &&
            Math.abs(old.across - stop.across) < 0.012 &&
            Math.abs(old.lift - stop.lift) < 0.012
          );
        })
          ? previous
          : { compact, stops },
      );
    };

    const observer = new ResizeObserver(() => {
      if (!frame) frame = requestAnimationFrame(measure);
    });
    observer.observe(element);
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
}: {
  host: RefObject<HTMLDivElement | null>;
  reduced: boolean;
  light: boolean;
  compact: boolean;
  stops: RoadStop[];
}) {
  const { progress, target } = useRoute(host, reduced);
  const palette = light ? LIGHT : DARK;
  const lastStop = stops.length - 1;
  const road = useMemo(() => buildRoad(stops, compact), [compact, stops]);
  const terrain = useMemo(() => buildTerrain(lastStop, road), [lastStop, road]);
  const tracers = useMemo(
    () => buildTracers(lastStop, road, terrain.height),
    [lastStop, road, terrain],
  );
  const phase = useRef(0);
  const visible = useRef(true);
  const traceMaterial = useRef<THREE.ShaderMaterial>(null);
  const uniforms = useMemo(
    () => ({
      focus: { value: new THREE.Vector2() },
      paper: { value: new THREE.Color(palette.paper) },
      ink: { value: new THREE.Color(palette.ink) },
      ruby: { value: new THREE.Color(palette.ruby) },
      stars: { value: new THREE.Color(palette.stars) },
      traceRoute: { value: tracers.route },
      traceSize: { value: tracers.size },
      flow: { value: 0 },
      lightMode: { value: Number(light) },
    }),
    [light, palette, tracers],
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

  useEffect(
    () => () => {
      terrain.geometry.dispose();
      tracers.geometry.dispose();
      tracers.route.dispose();
    },
    [terrain, tracers],
  );

  useFrame(({ camera, size, invalidate }, delta) => {
    const flowing = !reduced && visible.current;
    if (flowing) {
      phase.current = (phase.current + Math.min(delta, 0.06) * tracers.speed) % 1;
    }
    const material = traceMaterial.current;
    if (material) material.uniforms.flow.value = phase.current;

    const distance = Math.abs(progress.current - target.current);
    progress.current =
      distance < 0.002 || reduced
        ? target.current
        : THREE.MathUtils.damp(progress.current, target.current, 8, Math.min(delta, 0.06));
    const position = progress.current;
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
        args={[undefined, undefined, tracers.count]}
        geometry={tracers.geometry}
        frustumCulled={false}
      >
        <shaderMaterial
          {...traceShaders}
          ref={traceMaterial}
          uniforms={uniforms}
          side={THREE.DoubleSide}
          transparent
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
          <MapScene
            host={host}
            reduced={reduced}
            light={resolvedTheme === "light"}
            compact={layout.compact}
            stops={layout.stops}
          />
        </Canvas>
      )}
    </div>
  );
}
