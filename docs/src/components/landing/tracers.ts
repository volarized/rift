import {
  DataTexture,
  FloatType,
  InstancedBufferAttribute,
  PlaneGeometry,
  RGBAFormat,
  Vector2,
} from "three";
import type { buildSphereIntake } from "./sphere";
import { type buildRoad, STOP_SPACING, traceWidth } from "./terrain";

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
  intake: ReturnType<typeof buildSphereIntake>,
) {
  const span = lastStop + END_PADDING * 2;
  const count = Math.ceil(span * TRACES_PER_STOP);
  const finaleCount = count * 2;
  const routeLength = span * STOP_SPACING;
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
  const motion = new Float32Array(finaleCount * 4);
  const appearance = new Float32Array(finaleCount * 3);

  for (let i = 0; i < finaleCount; i++) {
    const spread = scatter(i, 15.31) + scatter(i, 29.17) + scatter(i, 23.25);
    motion[i * 4] = ((i % count) + scatter(i, 7.1)) / count;
    motion[i * 4 + 1] = spread / 3;
    motion[i * 4 + 2] = (5 * (0.0017 + scatter(i, 47.6) ** 2 * 0.0065)) / span;
    motion[i * 4 + 3] = 0.054 + scatter(i, 34.8) ** 2 * 0.32;
    appearance[i * 3] = 0.022 * (0.68 + scatter(i, 5.16) * 0.92);
    appearance[i * 3 + 1] = scatter(i, 8.2);
    appearance[i * 3 + 2] = i % intake.size.y;
  }

  const geometry = new PlaneGeometry(1, 1, 1, 3);
  const strands = new PlaneGeometry(1, 1, 1, 4);
  const sharedMotion = new InstancedBufferAttribute(motion, 4);
  const sharedAppearance = new InstancedBufferAttribute(appearance, 3);
  for (const traces of [geometry, strands]) {
    traces.setAttribute("traceMotion", sharedMotion);
    traces.setAttribute("traceAppearance", sharedAppearance);
  }

  return {
    count,
    finaleCount,
    geometry,
    strands,
    route,
    size: new Vector2(ALONG_SAMPLES, ACROSS_SAMPLES),
    speed: STOPS_PER_SECOND / span,
    routeLength,
  };
}

const pathUniforms = `
  attribute vec4 traceMotion;
  attribute vec3 traceAppearance;
  uniform sampler2D intakePaths;
  uniform vec2 intakeSize;
  uniform vec2 animation;
  uniform vec3 sphereCenter;
  uniform float approachSamples;
  uniform float routeLength;
  uniform float baseTraceCount;

  const vec3 viewDirection = vec3(0.0, 0.5773503, 0.8164966);

  float distanceFromFeed(float progress) {
    return (1.0 - progress) * routeLength;
  }

  float traceVisibility() {
    return mix(1.0, smoothstep(0.34, 0.94, animation.y), step(baseTraceCount, float(gl_InstanceID)));
  }

  vec4 readPath(float column) {
    vec2 texel = vec2(column, traceAppearance.z);
    return texture2D(intakePaths, (texel + 0.5) / intakeSize);
  }

  vec3 samplePath(float first, float last, float t) {
    float column = mix(first, last, clamp(t, 0.0, 1.0));
    return mix(readPath(floor(column)).xyz, readPath(ceil(column)).xyz, fract(column));
  }

  vec3 rotateAroundView(vec3 local, float angle) {
    float s = sin(angle);
    float c = cos(angle);
    return c * local + s * cross(viewDirection, local)
      + (1.0 - c) * dot(viewDirection, local) * viewDirection;
  }

  vec3 rollingShell(vec3 point, float weight) {
    float angle = weight * 0.18 * sin(animation.x * 12.56637);
    return sphereCenter + rotateAroundView(point - sphereCenter, angle);
  }
`;

export const traceShaders = {
  vertexShader: `
    ${pathUniforms}
    uniform sampler2D traceRoute;
    uniform vec2 traceSize;
    uniform float feedLength;
    uniform float sphereRadius;
    varying vec2 mapPoint;
    varying float alongTrace;
    varying float edgeFade;
    varying float seed;
    varying float intakeLight;

    vec3 readPoint(vec2 texel) {
      return texture2D(traceRoute, (texel + 0.5) / traceSize).xyz;
    }

    vec3 streamPoint(float progress) {
      vec2 texel = clamp(vec2(progress, traceMotion.y), 0.0, 1.0) * (traceSize - 1.0);
      vec2 lo = floor(texel);
      vec2 hi = min(lo + 1.0, traceSize - 1.0);
      vec2 weight = fract(texel);
      return mix(
        mix(readPoint(lo), readPoint(vec2(hi.x, lo.y)), weight.x),
        mix(readPoint(vec2(lo.x, hi.y)), readPoint(hi), weight.x),
        weight.y
      ) + vec3(0.0, traceMotion.w, 0.0);
    }

    vec3 routedPoint(float progress, float approachLength) {
      float distance = distanceFromFeed(progress);
      float t = clamp(distance / approachLength, 0.0, 1.0);
      float column = t * (approachSamples - 1.0);
      float previous = min(floor(column), approachSamples - 2.0);
      vec3 a = readPath(previous).xyz;
      vec3 b = readPath(previous + 1.0).xyz;
      vec3 point = mix(a, b, column - previous);
      vec3 across = cross(b - a, viewDirection);
      across /= max(length(across), 0.00001);
      float entering = smoothstep(max(feedLength, approachLength - 0.94 * sphereRadius), approachLength, distance);
      point = rollingShell(point, entering);
      float width = sphereRadius * (0.47 + 0.15 * cos(distance * 0.57));
      point += across * (traceMotion.y - 0.5) * width * (1.0 - entering);
      point += viewDirection * traceMotion.w * (1.0 - entering);
      if (animation.y == 1.0) return point;
      return mix(streamPoint(progress), point, animation.y);
    }

    void main() {
      // Progress increases uphill, so the shared phase carries particles downhill.
      float progress = fract(traceMotion.x - animation.x);
      float leadPhase = progress - traceMotion.z;
      // Double the trail while keeping the same head and sphere arrival.
      float tailPhase = leadPhase + 4.0 * traceMotion.z;
      float samplePhase = mix(leadPhase, tailPhase, position.y + 0.5);
      const float tangentStep = 0.00085;
      edgeFade = smoothstep(0.0, 0.072, min(samplePhase, 1.0 - samplePhase));
      seed = traceAppearance.y;
      vec3 point;
      vec3 next;
      intakeLight = 0.0;
      if (animation.y > 0.0) {
        float approachLength = readPath(0.0).w;
        float tailDistance = distanceFromFeed(tailPhase);
        if (animation.y == 1.0 && tailDistance >= approachLength) {
          gl_Position = vec4(2.0, 2.0, 2.0, 1.0);
          return;
        }
        float distance = distanceFromFeed(samplePhase);
        float handoff = smoothstep(approachLength - sphereRadius * 0.11, approachLength, distance);
        float feedFade = smoothstep(0.0, 1.1, distance) * (1.0 - handoff);
        edgeFade = mix(edgeFade, feedFade, animation.y);
        point = routedPoint(samplePhase, approachLength);
        next = routedPoint(samplePhase + tangentStep, approachLength);
        intakeLight = animation.y;
      } else {
        point = streamPoint(samplePhase);
        next = streamPoint(samplePhase + tangentStep);
      }

      vec3 sideways = cross(next - point, viewDirection);
      vec3 side = sideways / max(length(sideways), 0.00001);
      float width = traceAppearance.x * (1.0 + 0.24 * intakeLight);
      point += side * position.x * width;
      if (intakeLight > 0.0) {
        vec3 offset = point - sphereCenter;
        float depth = dot(offset, viewDirection);
        float rimDistance = length(offset - depth * viewDirection);
        float behindShell = (1.0 - step(0.0, depth)) *
          (1.0 - smoothstep(0.57 * sphereRadius, 1.01 * sphereRadius, rimDistance));
        edgeFade *= 1.0 - 0.78 * intakeLight * behindShell;
      }
      edgeFade *= traceVisibility();
      mapPoint = point.xz;
      alongTrace = position.y;
      gl_Position = projectionMatrix * modelViewMatrix * vec4(point, 1.0);
    }
  `,
  fragmentShader: `
    varying vec2 mapPoint;
    varying float alongTrace;
    varying float edgeFade;
    varying float seed;
    varying float intakeLight;
    uniform vec2 focus;
    uniform vec3 ruby;
    uniform vec3 stars;

    void main() {
      float amount = mix(1.0 - smoothstep(4.0, 36.0, length(mapPoint - focus)), 1.0, intakeLight);
      float tail = smoothstep(-0.5, -0.30, alongTrace) *
        (1.0 - smoothstep(-0.25, 0.5, alongTrace));
      vec3 color = mix(stars, ruby, pow(seed, 2.0) * 0.72);
      float brightness = mix(0.20, 0.99, pow(seed, 2.4));
      gl_FragColor = vec4(color, amount * tail * brightness * edgeFade);
      #include <colorspace_fragment>
    }
  `,
};

export const strandShaders = {
  vertexShader: `
    ${pathUniforms}
    varying float opacity;
    varying float facing;
    varying float across;
    varying float gust;
    varying float tint;

    const vec3 viewUp = vec3(0.0, 0.8164966, -0.5773503);

    vec3 shellPoint(float t) {
      return rollingShell(samplePath(approachSamples, intakeSize.x - 1.0, t), 1.0);
    }

    vec2 windAt(vec3 normal) {
      vec2 projected = vec2(normal.x, dot(normal, viewUp));
      float angle = atan(projected.y, projected.x + 0.000001);
      float depth = dot(normal, viewDirection);
      float clock = animation.x * 6.2831853;
      float curl = sin(3.0 * (angle - 3.0 * clock) + 2.8 * depth +
        1.06 * sin(2.0 * angle + 2.0 * clock));
      float eddy = sin(5.0 * angle - 7.0 * clock + 5.0 * depth);
      return vec2(curl, eddy) * smoothstep(0.08, 0.44, length(projected));
    }

    void main() {
      float progress = fract(traceMotion.x - animation.x) - traceMotion.z;
      float distance = distanceFromFeed(progress) - readPath(0.0).w;
      float lengthOnShell = readPath(approachSamples).w;
      float seed = traceAppearance.y;
      float trail = min(lengthOnShell, mix(0.54, 2.15, seed * seed));
      float formed = smoothstep(0.08, 0.98, animation.y);
      float head = min(distance, lengthOnShell);
      float tail = max(0.0, distance - trail * formed);
      if (head <= tail) {
        gl_Position = vec4(2.0, 2.0, 2.0, 1.0);
        return;
      }

      float t = mix(head, tail, position.y + 0.5) / lengthOnShell;
      vec3 point = shellPoint(t);
      vec3 direction = shellPoint(t + 0.002) - shellPoint(t - 0.002);
      vec3 local = point - sphereCenter;
      vec3 normal = normalize(local);
      vec2 wind = windAt(normal);
      // The gusts bank the trails after the particles join the shell.
      // Their phase is shared by both hemispheres and moves around the center.
      float inStorm = smoothstep(0.0, 0.065, t);
      float bank = inStorm * (0.16 * wind.x + 0.065 * wind.y + 0.12 * (traceMotion.y - 0.5));
      float lift = inStorm * (0.011 * wind.x + 0.007 * wind.y + 0.037 * (seed - 0.5));
      point = sphereCenter + rotateAroundView(local * (1.0 + lift), bank);
      direction = rotateAroundView(direction, bank);
      vec3 sideways = cross(direction, viewDirection);
      vec3 side = sideways / max(length(sideways), 0.00001);
      float age = clamp((distance - t * lengthOnShell) / trail, 0.0, 1.0);
      float taper = smoothstep(0.0, 0.12, age) * (1.0 - smoothstep(0.25, 1.0, age));
      facing = dot(normal, viewDirection);
      gust = smoothstep(-0.67, 0.88, 0.77 * wind.x + 0.23 * wind.y);
      tint = seed * seed;
      float sparkle = sqrt(min(1.0, traceMotion.w / 0.36));
      opacity = formed * traceVisibility() * taper * mix(0.37, 0.73, sparkle) * mix(0.55, 1.0, gust);
      across = position.x;
      point += side * position.x * traceAppearance.x * (0.68 + taper * (0.75 + 0.65 * gust));
      gl_Position = projectionMatrix * modelViewMatrix * vec4(point, 1.0);
    }
  `,
  fragmentShader: `
    varying float opacity;
    varying float facing;
    varying float across;
    varying float gust;
    varying float tint;
    uniform vec3 ruby;
    uniform vec3 stars;

    void main() {
      float front = smoothstep(-0.9, 0.4, facing);
      float softEdge = 1.0 - smoothstep(0.19, 0.5, abs(across));
      vec3 color = mix(stars, ruby, (0.14 + 0.33 * tint) * (1.0 - 0.64 * gust));
      gl_FragColor = vec4(color, opacity * softEdge * mix(0.47, 1.0, front));
      #include <colorspace_fragment>
    }
  `,
};
