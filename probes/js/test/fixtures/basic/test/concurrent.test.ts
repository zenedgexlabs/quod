import { expect, test } from 'vitest';
import { add } from '../src/calc';

test.concurrent('c1', async () => {
  expect(add(1, 1)).toBe(2);
});
test.concurrent('c2', async () => {
  expect(add(2, 2)).toBe(4);
});
