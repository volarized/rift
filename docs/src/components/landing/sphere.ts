import {
  CatmullRomCurve3,
  CubicBezierCurve3,
  CurvePath,
  DataTexture,
  FloatType,
  LineCurve3,
  RGBAFormat,
  Vector2,
  Vector3,
} from "three";

const STRANDS = 43;
const APPROACH_SAMPLES = 161;
const SPHERE_SAMPLES = 289;
const TURN = Math.PI * 2;
const GOLDEN_ANGLE = Math.PI * (3 - Math.sqrt(5));
const VIEW_UP = new Vector3(0, Math.sqrt(2 / 3), -Math.sqrt(1 / 3));
const VIEW_FORWARD = new Vector3(0, Math.sqrt(1 / 3), Math.sqrt(2 / 3));

export function buildSphereIntake(
  center: Vector3,
  radius: number,
  compact: boolean,
  view: Vector2,
  projectedDown: number,
) {
  const fork = center
    .clone()
    .addScaledVector(VIEW_UP, radius * (compact ? 1.36 : 1.48))
    .addScaledVector(VIEW_FORWARD, -radius * 1.52);
  if (!compact) fork.x += radius * 1.03;

  const screenPoint = (x: number, y: number) => {
    const down = projectedDown - radius * 1.48;
    const point = fork.clone().addScaledVector(VIEW_UP, down - (y - 0.5) * view.y);
    point.x = (x - 0.5) * view.x;
    return point;
  };
  const shoulder = fork.clone().addScaledVector(VIEW_UP, radius * 0.64);
  shoulder.x += radius * 0.62;
  // The shared stream ends above the rim so the lanes can widen before contact.
  const feed = compact
    ? new LineCurve3(fork.clone().addScaledVector(VIEW_UP, view.y * 0.86), fork)
    : new CubicBezierCurve3(screenPoint(0.41, -0.34), screenPoint(0.81, 0.065), shoulder, fork);
  const drive = feed.getTangent(1);
  const columns = APPROACH_SAMPLES + SPHERE_SAMPLES;
  const data = new Float32Array(columns * STRANDS * 4);

  const writeCurve = (
    curve: CurvePath<Vector3> | CatmullRomCurve3,
    offset: number,
    count: number,
  ) => {
    const length = curve.getLength();
    curve.getSpacedPoints(count - 1).forEach(({ x, y, z }, i) => {
      data.set([x, y, z, length], (offset + i) * 4);
    });
  };

  for (let strand = 0; strand < STRANDS; strand++) {
    const band = (2 * strand) / (STRANDS - 1) - 1;
    const variation = strand * GOLDEN_ANGLE;
    const latitude = band * 1.49 + 0.018 * Math.sin(variation);
    const tilt = -0.47 + 0.13 * Math.sin(variation) + 0.09 * band;
    const east = new Vector3(Math.cos(tilt), 0, 0).addScaledVector(VIEW_UP, Math.sin(tilt));
    const north = new Vector3(-Math.sin(tilt), 0, 0).addScaledVector(VIEW_UP, Math.cos(tilt));
    const entry = -Math.PI + 0.21 + (Math.PI - 0.42) * ((variation / TURN) % 1);
    const pointOnShell = (angle: number) => {
      const wave =
        0.68 * Math.sin(2 * angle + 1.38 * band + 0.46 * Math.cos(variation)) +
        0.25 * Math.cos(angle - 2 * band) +
        0.18 * Math.sin(3 * angle - variation);
      const bent = latitude + wave * 0.26 * Math.cos(latitude);
      const ring = Math.cos(bent);
      return center
        .clone()
        .addScaledVector(east, radius * ring * Math.cos(angle))
        .addScaledVector(north, radius * Math.sin(bent))
        .addScaledVector(VIEW_FORWARD, radius * ring * Math.sin(angle));
    };
    const seam = pointOnShell(entry);
    // Continue in the direction that follows the incoming trace at its seam.
    const incoming = seam.clone().sub(fork);
    const advancing = pointOnShell(entry + 0.006).sub(pointOnShell(entry - 0.006));
    const direction = advancing.dot(incoming) >= 0 ? 1 : -1;
    const points = Array.from({ length: 253 }, (_, index) =>
      pointOnShell(entry + direction * (index / 252) * TURN * 2.28),
    );

    const orbit = new CatmullRomCurve3(points);
    orbit.arcLengthDivisions = 512;
    const tangent = orbit.getTangent(0);
    const rearDepth = center.clone().sub(seam).dot(VIEW_FORWARD);
    const handoff = Math.min(
      radius * 0.59,
      (rearDepth * 0.8) / Math.max(0.00001, -tangent.dot(VIEW_FORWARD)),
    );
    const relative = seam.clone().sub(center);
    const lateral = Math.max(-0.92, Math.min(0.92, relative.x / radius));
    const lower = Math.max(0, -relative.dot(VIEW_UP) / radius);
    const angle = Math.asin(lateral) + 0.5 * lower * Math.sign(lateral);
    // Give each lane room to reach its part of the rear silhouette before
    // folding into the surface. A straight approach would overlap at the rim.
    const rim = center
      .clone()
      .addScaledVector(VIEW_UP, radius * 1.12 * Math.cos(angle))
      .addScaledVector(VIEW_FORWARD, -radius * 1.32);
    rim.x += radius * 1.12 * Math.sin(angle);
    const steering = rim
      .clone()
      .sub(fork)
      .normalize()
      .lerp(seam.clone().sub(rim).normalize(), 0.62)
      .normalize();
    const rimEntry = Math.min(radius * 0.75, fork.distanceTo(rim) * 0.44);
    const rimExit = Math.min(radius * 0.55, rim.distanceTo(seam) * 0.37);
    const approach = new CurvePath<Vector3>();
    approach.add(feed);
    approach.add(
      new CubicBezierCurve3(
        fork,
        fork.clone().addScaledVector(drive, Math.min(radius * 0.72, fork.distanceTo(rim) * 0.52)),
        rim.clone().addScaledVector(steering, -rimEntry),
        rim,
      ),
    );
    approach.add(
      new CubicBezierCurve3(
        rim,
        rim.clone().addScaledVector(steering, rimExit),
        seam.clone().addScaledVector(tangent, -handoff),
        seam,
      ),
    );
    writeCurve(approach, strand * columns, APPROACH_SAMPLES);
    writeCurve(orbit, strand * columns + APPROACH_SAMPLES, SPHERE_SAMPLES);
  }

  const paths = new DataTexture(data, columns, STRANDS, RGBAFormat, FloatType);
  paths.needsUpdate = true;
  return {
    paths,
    approachSamples: APPROACH_SAMPLES,
    size: new Vector2(columns, STRANDS),
    feedLength: feed.getLength(),
  };
}
