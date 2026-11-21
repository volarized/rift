import { Circle, Square, make, registry } from "./shapes";

export const fallback = make();

export function total(): number {
  const shape = make();
  return shape.area();
}

export function each(): number {
  let result = 0;
  for (const shape of [new Square(), new Square()]) {
    result += shape.area();
  }
  return result;
}

export function lookup(): number {
  return registry.circle.area();
}

export function mapped(): number[] {
  return [new Circle()].map((shape) => shape.area());
}

export function label(): string {
  return String("x".length).toUpperCase();
}
