import { beacon } from "./hub";

export interface Reading {
  level: number;
}

export function larger(): number {
  return Math.max(beacon(2), 1);
}
