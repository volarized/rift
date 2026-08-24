export interface Route {
  path: string;
}

export function lookup(route: Route): string {
  return route.path;
}
