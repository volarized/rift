import { beacon } from "./hub";

export function Banner() {
  return <span>{beacon(3)}</span>;
}
