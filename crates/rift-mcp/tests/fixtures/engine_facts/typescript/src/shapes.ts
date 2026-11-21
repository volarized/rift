export interface Shape {
  area(): number;
}

export class Square implements Shape {
  area(): number {
    return 1;
  }
}

export class Circle implements Shape {
  area(): number {
    return 3;
  }
}

export function make() {
  return new Square();
}

export const registry = { circle: new Circle() };
