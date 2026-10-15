import type { SVGProps } from "react";

// Source: https://upload.wikimedia.org/wikipedia/commons/c/c1/TOML_Logo.svg
const Toml = (props: SVGProps<SVGSVGElement>) => (
  <svg {...props} viewBox="0 0 400 400">
    <title>TOML logo</title>
    <rect width="400" height="400" fill="#fff" />
    <path fill="#7E7F7F" d="M28.6,30.5h75.1v37.3H69v262.7h34.7V368H28.6V30.5z" />
    <path fill="#000" d="M276.4,101.5v39.1h-60.1v181.9H174V140.6h-60.4v-39.1H276.4z" />
    <path fill="#7E7F7F" d="M365.7,368h-75.1v-37.3h34.7V68h-34.7V30.5h75.1V368z" />
  </svg>
);

export { Toml };
