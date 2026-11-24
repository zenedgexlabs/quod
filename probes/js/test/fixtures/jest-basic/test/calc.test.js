const { add, mul } = require('../src/calc');

describe('calc', () => {
  test('add', () => {
    expect(add(1, 2)).toBe(3);
  });
  test('mul', () => {
    expect(mul(2, 3)).toBe(6);
  });
});

test('reads env', () => {
  expect(process.env.FIXTURE_VAR).toBe('1');
});
