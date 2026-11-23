import { beforeAll, describe, expect, test } from 'vitest';
import { Box, add, mul } from '../src/calc';

let base = 0;
beforeAll(() => {
  base = add(0, 0);
});

describe('calc', () => {
  test('add', () => {
    expect(add(1, 2) + base).toBe(3);
  });
  test('mul', () => {
    expect(mul(2, 3)).toBe(12);
  });
  test.each([1, 2])('box %i', (n) => {
    expect(new Box().push(n).total).toBe(n);
  });
});

test('top level', () => {
  expect(true).toBe(true);
});
