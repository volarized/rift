import { CubicBezierCurve, Float32BufferAttribute, MathUtils, PlaneGeometry, Vector2 } from "three";
import { ImprovedNoise } from "three/addons/math/ImprovedNoise.js";

export const STOP_SPACING = 22;
const noise = new ImprovedNoise();

export type RoadStop = { across: number; lift: number };

export function viewHeight(width: number) {
  return width < 768 ? 24 : 27;
}

export function roadElevation(progress: number) {
  return -2.3 + progress * 2.85 + 0.34 * Math.sin(progress * 1.65);
}

export function buildRoad(stops: readonly RoadStop[], compact: boolean) {
  const last = stops.length - 1;
  const reach = compact ? 1.4 : 6.2;
  const handle = (index: number) => (index % 2 === 0 ? -reach : reach);
  const point = (index: number) => {
    const across =
      index < 0
        ? stops[0].across - handle(0)
        : index > last
          ? stops[last].across + handle(last)
          : stops[index].across;
    return new Vector2(across, -index * STOP_SPACING);
  };
  const curves = Array.from({ length: last + 4 }, (_, offset) => {
    const index = offset - 2;
    const start = point(index);
    const end = point(index + 1);
    return new CubicBezierCurve(
      start,
      new Vector2(start.x + handle(index), start.y - STOP_SPACING / 3),
      new Vector2(end.x - handle(index + 1), end.y + STOP_SPACING / 3),
      end,
    );
  });

  return (progress: number, target: Vector2) => {
    const segment = MathUtils.clamp(Math.floor(progress), -2, last + 1);
    return curves[segment + 2].getPoint(MathUtils.clamp(progress - segment, 0, 1), target);
  };
}

function mountainRelief(x: number, z: number, ascent: number) {
  const warp = x + 4.6 * noise.noise(x * 0.049, z * 0.064, 8);
  const ridge = z + 3.3 * noise.noise(x * 0.11, z * 0.053, 12);
  const east = Math.exp(-(((warp - 12 - 4 * Math.sin(ridge * 0.086)) / 8.3) ** 2));
  const west = Math.exp(-(((warp + 14 + 3.8 * Math.cos(ridge * 0.097)) / 9.2) ** 2));
  const eastCrest = 4.4 + 5.8 * (0.5 + 0.5 * Math.sin(ridge * 0.19 + 0.9)) ** 2;
  const westCrest = 4.8 + 6.2 * (0.5 + 0.5 * Math.cos(ridge * 0.16 + 2.6)) ** 2;
  const highlands = (east * eastCrest + west * westCrest) * (0.76 + ascent * 0.36);
  const texture =
    2.8 * noise.noise(x * 0.115, z * 0.107, 3) +
    1.05 * noise.noise(x * 0.235, z * 0.212, 16) +
    0.32 * noise.noise(x * 0.46, z * 0.45, 24) +
    0.1 * noise.noise(x * 0.89, z * 0.81, 42);

  return highlands + texture * (0.8 + 0.48 * Math.max(east, west)) - 0.7;
}

export function traceWidth(progress: number) {
  return 0.17 + 2.75 * Math.abs(Math.cos(progress * Math.PI)) ** 1.65;
}

export function buildTerrain(lastStop: number, road: ReturnType<typeof buildRoad>) {
  const routeLength = lastStop * STOP_SPACING;
  const depth = routeLength + 106;
  const columns = 312;
  const rows = Math.ceil(depth / 0.33);
  const pitch = columns + 1;
  const terrain = new PlaneGeometry(102, depth, columns, rows);
  terrain.rotateX(-Math.PI / 2);
  terrain.translate(0, 0, -routeLength / 2);

  const points = terrain.getAttribute("position");
  const distances = new Float32Array(points.count);
  const route = new Vector2();

  const crossSection = (along: number) => {
    const progress = -along / STOP_SPACING;
    const center = road(progress, route).x;
    const ascent = MathUtils.clamp(progress / lastStop, 0, 1);
    const base = roadElevation(progress);
    const height = (across: number) =>
      base +
      MathUtils.smoothstep(Math.abs(across - center), 0.06, 6.7) *
        mountainRelief(across, along, ascent);
    return { center, height, width: traceWidth(progress) };
  };

  for (let row = 0; row <= rows; row++) {
    const start = row * pitch;
    const { center, height, width } = crossSection(points.getZ(start));
    for (let col = 0; col <= columns; col++) {
      const index = start + col;
      const across = points.getX(index);
      points.setY(index, height(across));
      distances[index] = (across - center) / (width + 0.3);
    }
  }
  terrain.setAttribute("traceDistance", new Float32BufferAttribute(distances, 1));
  terrain.computeVertexNormals();

  return {
    geometry: terrain,
    height: (across: number, along: number) => crossSection(along).height(across),
  };
}

export const terrainShaders = {
  vertexShader: `
    attribute float traceDistance;
    varying float fromTrace;
    varying vec3 mapPoint;
    varying vec3 mapNormal;
    void main() {
      fromTrace = traceDistance;
      mapPoint = position;
      mapNormal = normal;
      gl_Position = projectionMatrix * modelViewMatrix * vec4(position, 1.0);
    }
  `,
  fragmentShader: `
    varying float fromTrace;
    varying vec3 mapPoint;
    varying vec3 mapNormal;
    uniform vec2 focus;
    uniform vec3 paper;
    uniform vec3 ink;
    uniform vec3 ruby;
    uniform float lightMode;
    uniform vec2 animation;

    float contour(float level) {
      float distanceToLine = abs(fract(level + 0.5) - 0.5);
      float pixel = max(0.012, fwidth(level));
      return (1.0 - smoothstep(pixel * 0.14, pixel * 1.25, distanceToLine)) *
        (1.0 - smoothstep(0.19, 0.47, pixel));
    }

    void main() {
      vec2 offset = (mapPoint.xz - focus) * vec2(0.85, 1.0);
      float distanceToFocus = length(offset);
      float spotlight = 1.0 - smoothstep(6.0, 36.0, distanceToFocus);
      float core = 1.0 - smoothstep(2.5, 21.0, distanceToFocus);
      vec3 normal = normalize(mapNormal);
      float facing = max(0.0, dot(normal, normalize(vec3(-0.66, 0.48, -0.56))));
      float slope = 1.0 - abs(normal.y);
      float relief = mix(0.014, 0.25, pow(facing, 1.65)) * (0.38 + slope * 0.62);
      vec3 ground = mix(paper, ink, relief * spotlight * mix(0.60, 1.0, lightMode));
      ground = mix(ground, ruby, core * (0.014 + 0.018 * slope));

      float heightBand = (mapPoint.y + 0.08) / 0.145;
      float fine = contour(heightBand);
      float indexLine = contour(heightBand * 0.2);
      vec3 lineColor = mix(ink, ruby, core * 0.30);
      float strength = (fine * 0.070 + indexLine * 0.13) *
        (0.13 + spotlight * 0.87) * (0.35 + facing * 0.65);
      vec3 color = mix(ground, lineColor, strength);

      float haze = exp(-fromTrace * fromTrace * 1.5) * (1.0 - animation.y);
      color = mix(color, mix(ink, ruby, 0.55), haze * spotlight * 0.022);

      gl_FragColor = vec4(color, 1.0);
      #include <colorspace_fragment>
    }
  `,
};
