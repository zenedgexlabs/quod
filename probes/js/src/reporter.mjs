// Quod Vitest reporter: merges the workers' per-module coverage records
// with test results into a capture file (protocol v1, ADR 013), and writes
// per-test results for `quod verdict`.

import path from 'node:path';
import { createRequire } from 'node:module';
import { writeCapture, writeResults } from './capture.mjs';

const slash = (p) => p.split(path.sep).join('/');

export default class QuodReporter {
  constructor(options = {}) {
    this.root = path.resolve(options.root || process.env.QUOD_ROOT || process.cwd());
    this.dir = options.dir || process.env.QUOD_RECORD_DIR;
    this.out = options.out || process.env.QUOD_RECORD_OUT;
    this.rootFiles = options.rootFiles || JSON.parse(process.env.QUOD_ROOT_FILES || '[]');
  }

  onInit(vitest) {
    this.vitest = vitest;
  }

  /** Vitest's results as the shared test list. */
  tests(testModules) {
    const out = [];
    for (const mod of testModules) {
      const file = slash(path.relative(this.root, mod.moduleId));
      for (const test of mod.children.allTests()) {
        const state = test.result().state;
        out.push({
          file,
          name: `${file}::${test.fullName}`,
          qual: test.fullName,
          outcome: state === 'passed' ? 'passed' : state === 'failed' ? 'failed' : 'skipped',
          duration_ms: test.diagnostic?.()?.duration ?? 0,
        });
      }
    }
    return out;
  }

  onTestRunEnd(testModules) {
    const tests = this.tests(testModules);
    writeResults(tests);
    if (!this.dir || !this.out) return;
    let runner = 'vitest';
    try {
      const require = createRequire(path.join(this.root, 'package.json'));
      runner = `vitest@${require('vitest/package.json').version}`;
    } catch {
      // keep the bare name
    }
    writeCapture({ root: this.root, dir: this.dir, out: this.out, rootFiles: this.rootFiles, runner, tests });
  }
}
