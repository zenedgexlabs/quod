import { expect, test, vi } from 'vitest';

test('reads env', () => {
  expect(process.env.FIXTURE_VAR).toBe('1');
  expect('FIXTURE_MISSING' in process.env).toBe(false);
});

test('stubs env', () => {
  vi.stubEnv('FIXTURE_STUB', 'x');
  expect(process.env.FIXTURE_STUB).toBe('x');
});

test('enumerates env', () => {
  expect(Object.keys(process.env).length).toBeGreaterThan(0);
});

test('reads runner env', () => {
  expect(process.env.VITEST).toBeTruthy();
  expect(process.env.TEST).toBeTruthy();
});
