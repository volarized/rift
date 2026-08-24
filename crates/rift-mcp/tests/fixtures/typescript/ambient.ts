declare namespace JSX {
  interface IntrinsicElements {
    [element: string]: Record<string, unknown>;
  }
}

interface String {
  first(): string;
}
