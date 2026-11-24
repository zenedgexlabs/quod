// Quod Jest test environment: wraps the project's own environment (node,
// jsdom or custom; QUOD_JEST_BASE_ENV), and hands the shared recorder to the
// test sandbox.
//
// Jest loads environments natively in the worker's outer realm, so the
// recorder keeps one inspector session per worker. Coverage starts in the
// constructor, before Jest evaluates the test module.
//
// CommonJS without top-level await: Jest 29 loads environments with
// require(), and the shared recorder (an ES module without top-level
// await) loads through Node's require(esm) (Node 22.12+).

'use strict';

const path = require('node:path');
const { createRecorder, readSkipFile } = require('./recorder.mjs');

const ROOT = path.resolve(process.env.QUOD_ROOT || process.cwd());
const DIR = path.resolve(process.env.QUOD_DIR || ROOT);

function resolveBase(spec) {
  const { createRequire } = require('node:module');
  const req = createRequire(path.join(DIR, 'package.json'));
  const name = !spec || spec === 'node' ? 'jest-environment-node' : spec === 'jsdom' ? 'jest-environment-jsdom' : spec;
  for (const candidate of [name, `jest-environment-${name}`]) {
    try {
      return req.resolve(candidate);
    } catch {
      // try the next spelling
    }
  }
  return req.resolve(path.resolve(DIR, name));
}

const mod = require(resolveBase(process.env.QUOD_JEST_BASE_ENV));
const Base = mod.TestEnvironment ?? mod.default?.TestEnvironment ?? mod.default ?? mod;

class QuodEnvironment extends Base {
  constructor(config, context) {
    super(config, context);
    const recordDir = process.env.QUOD_RECORD_DIR;
    const recorder = recordDir ? createRecorder({ root: ROOT, recordDir, proc: this.global.process }) : null;
    // Coverage must be running before Jest evaluates the test module.
    if (recorder) recorder.start();
    this.global.__quodRuntime = { recorder, readSkipFile };
  }
}

module.exports = QuodEnvironment;
module.exports.default = QuodEnvironment;
module.exports.TestEnvironment = QuodEnvironment;
