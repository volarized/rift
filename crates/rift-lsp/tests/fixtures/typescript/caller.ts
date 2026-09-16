import { beacon } from "./hub";
import { Banner } from "./view";

export function total(): number {
  return beacon(2);
}

export const heading = Banner;
