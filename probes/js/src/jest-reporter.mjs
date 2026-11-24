// Quod Jest reporter: the capture file (protocol v1) from the workers'
// module records, and per-test results for `quod verdict`.

import path from 'node:path';
import { createRequire } from 'node:module';
import { writeCapture, writeResults } from './capture.mjs';

const slash = (p) => p.split(path.sep).join('/');

export default class QuodJestReporter {
  constructor(globalConfig, options = {}) {
    this.root = path.resolve(options.root || process.env.QUOD_ROOT || process.cwd());
    this.dir = options.dir || process.env.QUOD_RECORD_DIR;
    this.out = options.out || process.env.QUOD_RECORD_OUT;
    this.rootFiles = options.rootFiles || JSON.parse(process.env.QUOD_ROOT_FILES || '[]');
  }

  /** Jest's results as the shared test list (`file::suite > test`). */
  tests(results) {
    const out = [];
    for (const file of results.testResults) {
      const rel = slash(path.relative(this.root, file.testFilePath));
      for (const a of file.testResults) {
        const qual = [...a.ancestorTitles, a.title].join(' > ');
        out.push({
          file: rel,
          name: `${rel}::${qual}`,
          qual,
          outcome: a.status === 'passed' ? 'passed' : a.status === 'failed' ? 'failed' : 'skipped',
          duration_ms: a.duration ?? 0,
        });
      }
      // A file that failed to run at all (syntax error, failing hook)
      // leaves no test results: nothing to record.
    }
    return out;
  }

  onRunComplete(_contexts, results) {
    const tests = this.tests(results);
    writeResults(tests);
    if (!this.dir || !this.out) return;
    let runner = 'jest';
    try {
      const require = createRequire(path.join(this.root, 'package.json'));
      runner = `jest@${require('jest/package.json').version}`;
    } catch {
      // keep the bare name
    }
    writeCapture({ root: this.root, dir: this.dir, out: this.out, rootFiles: this.rootFiles, runner, tests });
  }
}
