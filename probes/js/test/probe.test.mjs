// Probe tests: unit tests of the coverage math, and one recorded Vitest run
// of the fixture project shared by the end-to-end checks.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { before, describe, test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { decodeMappings, executedFunctions, executedMask } from '../src/coverage.mjs';
import { main } from '../src/cli.mjs';
import { digest, environHash, envValueHash } from '../src/fingerprint.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const FIXTURE = path.join(HERE, 'fixtures', 'basic');

describe('coverage math', () => {
  test('decodes VLQ mappings', () => {
    // "AAAA" = [0,0,0,0]; "CAAC" = [+1,0,0,+1]; ";" starts a new line.
    assert.deepEqual(decodeMappings('AAAA,CAAC;AACA'), [
      [
        [0, 0, 0, 0],
        [1, 0, 0, 1],
      ],
      [[0, 0, 1, 1]],
    ]);
  });

  test('nested ranges decide what ran', () => {
    const fns = [
      { ranges: [{ startOffset: 0, endOffset: 10, count: 1 }] },
      { ranges: [{ startOffset: 2, endOffset: 5, count: 0 }] },
    ];
    assert.deepEqual([...executedMask(fns, 10)], [1, 1, 0, 0, 0, 1, 1, 1, 1, 1]);
  });

  test('function starts map through the source map; wrappers are module-level', () => {
    const code = 'const a = 1;\nfunction f() {}\n';
    const scriptSource = 'WRAP(' + code + ')';
    const functions = [
      { functionName: '', ranges: [{ startOffset: 0, endOffset: scriptSource.length, count: 1 }] },
      { functionName: 'f', ranges: [{ startOffset: 5 + 13, endOffset: 5 + 28, count: 1 }] },
      { functionName: 'g', ranges: [{ startOffset: 5 + 20, endOffset: 5 + 22, count: 0 }] },
    ];
    // Identity map: line 1 -> 1, line 2 -> 2 (original lines shifted by 2).
    const map = { mappings: 'AAEA;AACA', sources: ['x.ts'] };
    const res = executedFunctions({ functions, scriptSource, code, map });
    assert.equal(res.module, true);
    assert.deepEqual(res.starts, [[4, 0]]);
  });
});

describe('recorded Vitest run', () => {
  let tests;
  let header;
  before(() => {
    const out = path.join(fs.mkdtempSync(path.join(os.tmpdir(), 'skipcert-js-')), 'cap.jsonl');
    process.env.FIXTURE_VAR = '1';
    delete process.env.FIXTURE_MISSING;
    const status = main(['record', '--out', out, '--root', FIXTURE]);
    assert.equal(status, 0);
    const recs = fs.readFileSync(out, 'utf8').trim().split('\n').map((l) => JSON.parse(l));
    header = recs[0];
    tests = Object.fromEntries(recs.filter((r) => r.type === 'test').map((r) => [r.name, r]));
  });

  const positions = (t, file) => (t.positions.find(([f]) => f === file)?.[1] ?? []).map(([l, c]) => `${l}:${c}`);

  test('header and names', () => {
    assert.equal(header.probe, 'js-probe/0.1.0');
    assert.equal(header.ecosystem, 'npm');
    assert.deepEqual(Object.keys(tests).sort(), [
      'test/calc.test.ts::calc > add',
      'test/calc.test.ts::calc > box 1',
      'test/calc.test.ts::calc > box 2',
      'test/calc.test.ts::calc > mul',
      'test/calc.test.ts::top level',
      'test/concurrent.test.ts::c1',
      'test/concurrent.test.ts::c2',
      'test/env.test.ts::enumerates env',
      'test/env.test.ts::reads env',
      'test/env.test.ts::reads runner env',
      'test/env.test.ts::stubs env',
    ]);
  });

  test('environment fingerprint in the header', () => {
    assert.deepEqual(header.capabilities, ['js.environment']);
    assert.equal(header.environment['interpreter.version'], process.version);
    assert.ok(header.environment['dist.npm:vitest'], 'installed packages are fingerprinted');
    assert.equal(header.environment_digest, digest(header.environment));
    assert.equal(header.invocation['invocation.args'], '[]');
  });

  test('process.env reads are recorded per test', () => {
    const env = (name) => Object.fromEntries(tests[name].env);
    const read = env('test/env.test.ts::reads env');
    assert.equal(read.FIXTURE_VAR, envValueHash('1'));
    assert.equal(read.FIXTURE_MISSING, envValueHash(undefined));
    assert.ok(!('*' in read));
    assert.ok(!('FIXTURE_STUB' in env('test/env.test.ts::stubs env')), 'a stubbed variable is not an input');
    assert.ok('*' in env('test/env.test.ts::enumerates env'));
    assert.ok(!('FIXTURE_VAR' in env('test/calc.test.ts::calc > add')));
  });

  test('env reads are recorded against the environment select sees', () => {
    const env = (name) => Object.fromEntries(tests[name].env);
    // Vitest sets these in its workers; this process (as `select`) has neither.
    const runner = env('test/env.test.ts::reads runner env');
    assert.equal(runner.VITEST, envValueHash(process.env.VITEST));
    assert.equal(runner.TEST, envValueHash(process.env.TEST));
    assert.equal(env('test/env.test.ts::enumerates env')['*'], environHash());
  });

  test('skip-file checks fail open to running everything', () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'skipcert-js-'));
    const run = (header) => {
      const skip = path.join(dir, 'skip.txt');
      fs.writeFileSync(skip, [...header, 'test/calc.test.ts::calc > mul', ''].join('\n'));
      const out = path.join(dir, `res-${Math.random()}.json`);
      main(['run', '--skip', skip, '--root', FIXTURE, '--', '--reporter=json', `--outputFile=${out}`]);
      const res = JSON.parse(fs.readFileSync(out, 'utf8'));
      return Object.fromEntries(res.testResults.flatMap((f) => f.assertionResults.map((a) => [a.fullName, a.status])))[
        'calc mul'
      ];
    };
    const good = [
      `# skipcert-environment ${header.environment_digest}`,
      `# skipcert-invocation ${digest({ ...header.invocation, 'invocation.args': JSON.stringify(['--reporter=json']) })}`,
    ];
    // The invocation differs (--outputFile), so nothing is skipped ...
    assert.equal(run(good), 'passed');
    // ... and a changed variable a skipped test read disables skipping too.
    assert.equal(run([`# skipcert-environment ${header.environment_digest}`, '# skipcert-env FIXTURE_VAR 00']), 'passed');
    // With only matching checks, the test is skipped.
    assert.equal(run([`# skipcert-environment ${header.environment_digest}`, `# skipcert-env FIXTURE_VAR ${envValueHash('1')}`]), 'skipped');
  });

  test('a test observes the functions it ran, not the others', () => {
    const mulT = positions(tests['test/calc.test.ts::calc > mul'], 'src/calc.ts');
    assert.ok(mulT.includes('5:19'), mulT.join(' ')); // mul's arrow
    assert.ok(!mulT.includes('15:7'), 'unused never ran');
    const box = positions(tests['test/calc.test.ts::calc > box 1'], 'src/calc.ts');
    assert.ok(box.includes('13:2'), box.join(' ')); // Box.push (shares its line with size)
    assert.ok(!box.some((p) => p.startsWith('13:5') && p !== '13:2'), box.join(' '));
  });

  test('beforeAll and module evaluation are shared by every test in the module', () => {
    for (const name of ['test/calc.test.ts::calc > mul', 'test/calc.test.ts::top level']) {
      const t = tests[name];
      assert.ok(positions(t, 'src/calc.ts').includes('2:7'), `${name} gets beforeAll's add`);
      assert.ok(t.files.includes('src/calc.ts'), `${name} gets module evaluation`);
    }
  });

  test('recording serialises concurrent tests, so they get their own windows', () => {
    const c1 = tests['test/concurrent.test.ts::c1'];
    assert.deepEqual(c1.flags, []);
    assert.ok(positions(c1, 'src/calc.ts').includes('2:7'));
    assert.deepEqual(tests['test/calc.test.ts::calc > add'].flags, []);
  });

  test('skip mode skips the named tests', () => {
    const skip = path.join(fs.mkdtempSync(path.join(os.tmpdir(), 'skipcert-js-')), 'skip.txt');
    fs.writeFileSync(skip, 'test/calc.test.ts::calc > mul\ntest/concurrent.test.ts::c2\n');
    const out = path.join(path.dirname(skip), 'res.json');
    const status = main(['run', '--skip', skip, '--root', FIXTURE, '--', '--reporter=json', `--outputFile=${out}`]);
    assert.equal(status, 0);
    const res = JSON.parse(fs.readFileSync(out, 'utf8'));
    const states = Object.fromEntries(
      res.testResults.flatMap((f) => f.assertionResults.map((a) => [a.fullName, a.status])),
    );
    assert.equal(states['calc mul'], 'skipped');
    assert.equal(states['c2'], 'skipped');
    assert.equal(states['calc add'], 'passed');
  });
});
