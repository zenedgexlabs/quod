import { CONST } from '../src/consts';
import { test, expect } from 'vitest';
test('a', () => {
  expect(CONST).toBe(2);
});
