// Quod Jest test environment: wraps the project's own environment (node,
// jsdom or custom; QUOD_JEST_BASE_ENV), and hands the shared recorder to the
// test sandbox.
//
// Jest loads environments natively in the worker's outer realm, so the
// recorder keeps one inspector session per worker. Coverage starts in the
// constructor, before Jest evaluates the test module.

import { createRequire } from 'node:module';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { createRecorder, readSkipFile } from './recorder.mjs';

const ROOT = path.resolve(process.env.QUOD_ROOT || process.cwd());
const DIR = path.resolve(process.env.QUOD_DIR || ROOT);

function resolveBase(spec) {
  const require = createRequire(path.join(DIR, 'package.json'));
  const name = !spec || spec === 'node' ? 'jest-environment-node' : spec === 'jsdom' ? 'jest-environment-jsdom' : spec;
  for (const candidate of [name, `jest-environment-${name}`]) {
    try {
      return require.resolve(candidate);
    } catch {
      // try the next spelling
    }
  }
  return require.resolve(path.resolve(DIR, name));
}

const mod = await import(pathToFileURL(resolveBase(process.env.QUOD_JEST_BASE_ENV)).href);
const Base = mod.TestEnvironment ?? mod.default?.TestEnvironment ?? mod.default?.default ?? mod.default;

export default class QuodEnvironment extends Base {
  constructor(config, context) {
    super(config, context);
    const recordDir = process.env.QUOD_RECORD_DIR;
    const recorder = recordDir ? createRecorder({ root: ROOT, recordDir, proc: this.global.process }) : null;
    // Coverage must be running before Jest evaluates the test module.
    recorder?.start();
    this.global.__quodRuntime = { recorder, readSkipFile };
  }
}
