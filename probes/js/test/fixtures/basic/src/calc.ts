export const RATE = 2;
export function add(a: number, b: number): number {
  return a + b;
}
export const mul = (a: number, b: number): number => {
  if (a > 100) {
    return 0;
  }
  return a * b * RATE;
};
export class Box {
  total = 0;
  push(n: number) { this.total += n; return this; } size() { return this.total; }
}
export function unused(): number {
  return 42;
}
