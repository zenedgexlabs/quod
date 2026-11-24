// Quod Jest worker setup (setupFilesAfterEnv, ADR 013). Must be the first
// entry: its hooks bound each test's coverage window.
//
// Jest evaluates this file inside the test file's sandbox. The shared
// recorder comes from the Quod test environment (jest-env.mjs), which Jest
// loads natively in the worker, so there is one inspector session per
// worker and coverage starts before the test module is evaluated.
//
// Names: Jest exposes the running test only as its titles joined by
// spaces (`expect.getState().currentTestName`). The describe/test globals
// are wrapped to record each test's title path at definition time, so
// coverage is keyed by `file::suite > test` like the reporter's names. A
// test whose name can't be recovered (`.each` titles, colliding names)
// leaves its module untraced: its tests always run.
//
// Skip mode (QUOD_SKIP_FILE): tests named in the file are defined with
// `test.skip`, so neither they nor their hooks run.

/* global beforeEach, afterEach, afterAll, expect */
'use strict';

const path = require('node:path');

const runtime = globalThis.__quodRuntime;
if (!runtime) throw new Error('quod: the Quod test environment is not active (run Jest through quod-js)');
const { recorder, readSkipFile } = runtime;

const ROOT = path.resolve(process.env.QUOD_ROOT || process.cwd());
const RECORD_DIR = process.env.QUOD_RECORD_DIR;
const SKIP_FILE = process.env.QUOD_SKIP_FILE;
const slash = (p) => p.split(path.sep).join('/');
const testFile = () => slash(path.relative(ROOT, expect.getState().testPath || ''));

// --- names ----------------------------------------------------------------

const stack = [];
/** Space-joined Jest test ID → `suite > test`, or null when ambiguous. */
const byId = new Map();
let usedConcurrent = false;
const skip = SKIP_FILE ? readSkipFile(SKIP_FILE) : null;

function register(title) {
  if (typeof title !== 'string') return null;
  const parts = [...stack, title];
  const id = parts.join(' ');
  const full = parts.join(' > ');
  byId.set(id, byId.has(id) && byId.get(id) !== full ? null : full);
  return full;
}

function copyProps(from, to) {
  for (const k of Object.keys(from)) if (!(k in to)) to[k] = from[k];
  return to;
}

function wrapDescribe(orig) {
  if (typeof orig !== 'function') return orig;
  const wrapped = function (title, fn, ...rest) {
    if (typeof fn !== 'function') return orig.call(this, title, fn, ...rest);
    return orig.call(
      this,
      title,
      function (...a) {
        stack.push(String(title));
        try {
          return fn.apply(this, a);
        } finally {
          stack.pop();
        }
      },
      ...rest,
    );
  };
  return wrapped;
}

function wrapTest(orig, { concurrent = false } = {}) {
  if (typeof orig !== 'function') return orig;
  return function (title, ...rest) {
    if (concurrent) usedConcurrent = true;
    const full = register(title);
    if (skip && full && skip.has(`${testFile()}::${full}`) && typeof orig.skip === 'function') {
      return orig.skip.call(this, title, ...rest);
    }
    return orig.call(this, title, ...rest);
  };
}

for (const name of ['describe', 'fdescribe', 'xdescribe']) {
  const orig = globalThis[name];
  if (typeof orig !== 'function') continue;
  const w = copyProps(orig, wrapDescribe(orig));
  for (const k of ['only', 'skip']) if (typeof orig[k] === 'function') w[k] = copyProps(orig[k], wrapDescribe(orig[k]));
  globalThis[name] = w;
}
for (const name of ['test', 'it', 'fit', 'xit', 'xtest']) {
  const orig = globalThis[name];
  if (typeof orig !== 'function') continue;
  const w = copyProps(orig, wrapTest(orig));
  for (const k of ['only', 'skip', 'failing']) if (typeof orig[k] === 'function') w[k] = copyProps(orig[k], wrapTest(orig[k]));
  if (typeof orig.concurrent === 'function') {
    w.concurrent = copyProps(orig.concurrent, wrapTest(orig.concurrent, { concurrent: true }));
  }
  globalThis[name] = w;
}

function currentName() {
  const full = byId.get(expect.getState().currentTestName);
  return full ? `${testFile()}::${full}` : null;
}

// --- record mode ------------------------------------------------------------

if (RECORD_DIR && recorder) {
  const mod = recorder.module();
  beforeEach(async () =>
    (await mod).before(currentName(), {
      moduleFile: expect.getState().testPath,
      concurrent: usedConcurrent && process.env.QUOD_SERIAL !== '1',
    }),
  );
  afterEach(async () => (await mod).after(currentName()));
  afterAll(async () => (await mod).finish());
}
