import {
  DataTexture,
  FloatType,
  InstancedBufferAttribute,
  PlaneGeometry,
  RGBAFormat,
  Vector2,
} from "three";
import { type buildRoad, traceWidth } from "./terrain";

const ALONG_SAMPLES = 1024;
const ACROSS_SAMPLES = 49;
const END_PADDING = 1.6;
const TRACES_PER_STOP = 620;
const STOPS_PER_SECOND = 0.22;

function scatter(index: number, phase: number) {
  const value = Math.sin(index * 12.9898 + phase) * 43758.5453;
  return value - Math.floor(value);
}

export function buildTracers(
  lastStop: number,
  road: ReturnType<typeof buildRoad>,
  height: (across: number, along: number) => number,
) {
  const span = lastStop + END_PADDING * 2;
  const count = Math.ceil(span * TRACES_PER_STOP);
  const surface = new Float32Array(ALONG_SAMPLES * ACROSS_SAMPLES * 4);
  const coordinates = new Vector2();

  // The shader samples this surface in route coordinates so thousands of
  // streaks can follow the camera's terrain without CPU updates per frame.
  for (let column = 0; column < ALONG_SAMPLES; column++) {
    const progress = (column / (ALONG_SAMPLES - 1)) * span - END_PADDING;
    const { x, y: z } = road(progress, coordinates);
    const width = traceWidth(progress);
    for (let row = 0; row < ACROSS_SAMPLES; row++) {
      const across = x + (3 * (row / (ACROSS_SAMPLES - 1)) - 1.5) * width;
      const offset = (row * ALONG_SAMPLES + column) * 4;
      surface[offset] = across;
      surface[offset + 1] = height(across, z);
      surface[offset + 2] = z;
    }
  }

  const route = new DataTexture(surface, ALONG_SAMPLES, ACROSS_SAMPLES, RGBAFormat, FloatType);
  route.needsUpdate = true;
  const motion = new Float32Array(count * 4);
  const appearance = new Float32Array(count * 2);

  for (let i = 0; i < count; i++) {
    const spread = scatter(i, 15.31) + scatter(i, 29.17) + scatter(i, 23.25);
    motion[i * 4] = (i + scatter(i, 7.1)) / count;
    motion[i * 4 + 1] = spread / 3;
    motion[i * 4 + 2] = (5 * (0.0017 + scatter(i, 47.6) ** 2 * 0.0065)) / span;
    motion[i * 4 + 3] = 0.054 + scatter(i, 34.8) ** 2 * 0.32;
    appearance[i * 2] = 0.022 * (0.68 + scatter(i, 5.16) * 0.92);
    appearance[i * 2 + 1] = scatter(i, 8.2);
  }

  const geometry = new PlaneGeometry(1, 1);
  geometry.setAttribute("traceMotion", new InstancedBufferAttribute(motion, 4));
  geometry.setAttribute("traceAppearance", new InstancedBufferAttribute(appearance, 2));

  return {
    count,
    geometry,
    route,
    size: new Vector2(ALONG_SAMPLES, ACROSS_SAMPLES),
    speed: STOPS_PER_SECOND / span,
  };
}

export const traceShaders = {
  vertexShader: `
    attribute vec4 traceMotion;
    attribute vec2 traceAppearance;
    uniform sampler2D traceRoute;
    uniform vec2 traceSize;
    uniform float flow;
    varying vec2 mapPoint;
    varying float alongTrace;
    varying float edgeFade;
    varying float seed;

    vec3 readPoint(vec2 texel) {
      return texture2D(traceRoute, (texel + 0.5) / traceSize).xyz;
    }

    vec3 sampleRoute(float progress, float across) {
      vec2 texel = clamp(vec2(progress, across), 0.0, 1.0) * (traceSize - 1.0);
      vec2 lo = floor(texel);
      vec2 hi = min(lo + 1.0, traceSize - 1.0);
      vec2 weight = fract(texel);
      return mix(
        mix(readPoint(lo), readPoint(vec2(hi.x, lo.y)), weight.x),
        mix(readPoint(vec2(lo.x, hi.y)), readPoint(hi), weight.x),
        weight.y
      );
    }

    void main() {
      // Progress increases uphill, so subtracting the phase moves the
      // particles toward the bottom of the viewport.
      float progress = fract(traceMotion.x - flow);
      vec3 lead = sampleRoute(progress - traceMotion.z, traceMotion.y);
      vec3 tail = sampleRoute(progress + traceMotion.z, traceMotion.y);
      vec3 axis = normalize(tail - lead);
      vec3 side = normalize(cross(axis, vec3(0.0, 0.5773503, 0.8164966)));
      vec3 point = mix(lead, tail, position.y + 0.5) + side * position.x * traceAppearance.x;
      point.y += traceMotion.w;
      mapPoint = point.xz;
      alongTrace = position.y;
      seed = traceAppearance.y;
      edgeFade = smoothstep(0.0, 0.072, min(progress, 1.0 - progress));
      gl_Position = projectionMatrix * modelViewMatrix * vec4(point, 1.0);
    }
  `,
  fragmentShader: `
    varying vec2 mapPoint;
    varying float alongTrace;
    varying float edgeFade;
    varying float seed;
    uniform vec2 focus;
    uniform vec3 ruby;
    uniform vec3 stars;

    void main() {
      float amount = 1.0 - smoothstep(4.0, 36.0, length(mapPoint - focus));
      float tail = smoothstep(-0.5, -0.30, alongTrace) *
        (1.0 - smoothstep(-0.25, 0.5, alongTrace));
      vec3 color = mix(stars, ruby, pow(seed, 2.0) * 0.72);
      float brightness = mix(0.20, 0.94, pow(seed, 2.4));
      gl_FragColor = vec4(color, amount * tail * brightness * edgeFade);
      #include <colorspace_fragment>
    }
  `,
};
