// Quod Vitest reporter: merges the workers' per-module coverage records
// with test results into a capture file (protocol v1, `lines` form, ADR 013).

import fs from 'node:fs';
import path from 'node:path';
import { createRequire } from 'node:module';
import { PROBE_ID, CAPTURE_VERSION } from './version.mjs';
import { digest } from './fingerprint.mjs';

const slash = (p) => p.split(path.sep).join('/');

export default class QuodReporter {
  constructor(options = {}) {
    this.root = path.resolve(options.root || process.env.QUOD_ROOT || process.cwd());
    this.dir = options.dir || process.env.QUOD_RECORD_DIR;
    this.out = options.out || process.env.QUOD_RECORD_OUT;
    this.rootFiles = options.rootFiles || JSON.parse(process.env.QUOD_ROOT_FILES || '[]');
    this.testDir = path.resolve(process.env.QUOD_DIR || this.root);
    this.args = JSON.parse(process.env.QUOD_ARGS || '[]');
  }

  onInit(vitest) {
    this.vitest = vitest;
  }

  /**
   * Per-test outcomes for `quod verdict` (QUOD_RESULTS_OUT), named exactly
   * as recorded. Tests Quod skipped (the skip file) are not results: they
   * carry certificates instead.
   */
  writeResults(testModules) {
    const out = process.env.QUOD_RESULTS_OUT;
    if (!out) return;
    const certified = new Set();
    try {
      const skip = process.env.QUOD_SKIP_FILE;
      if (skip) {
        for (const line of fs.readFileSync(skip, 'utf8').split(/\r?\n/)) {
          if (line && !line.startsWith('#')) certified.add(line);
        }
      }
    } catch {
      // no skip file: every test is a result
    }
    const lines = [];
    for (const mod of testModules) {
      const file = slash(path.relative(this.root, mod.moduleId));
      for (const test of mod.children.allTests()) {
        const name = `${file}::${test.fullName}`;
        if (certified.has(name)) continue;
        const state = test.result().state;
        const outcome = state === 'passed' ? 'passed' : state === 'failed' ? 'failed' : 'skipped';
        const duration = test.diagnostic?.()?.duration ?? 0;
        lines.push(JSON.stringify({ duration_ms: Math.round(duration * 1000) / 1000, name, outcome, type: 'result' }));
      }
    }
    lines.sort();
    fs.writeFileSync(out, lines.length ? `${lines.join('\n')}\n` : '');
  }

  onTestRunEnd(testModules) {
    this.writeResults(testModules);
    if (!this.dir || !this.out) return;
    const records = [];
    for (const f of fs.existsSync(this.dir) ? fs.readdirSync(this.dir) : []) {
      if (f.endsWith('.json') && f !== 'fingerprint.json') records.push(JSON.parse(fs.readFileSync(path.join(this.dir, f), 'utf8')));
    }
    // A module can run in several workers (retries, projects): union them.
    const byModule = new Map();
    const packages = new Map();
    for (const r of records) {
      for (const p of r.packages) packages.set(p.name, p);
      const m = byModule.get(r.module) ?? { concurrent: false, shared: [], tests: new Map() };
      m.concurrent ||= r.concurrent;
      m.shared.push(r.shared);
      for (const [name, cov] of Object.entries(r.tests)) {
        const list = m.tests.get(name) ?? [];
        list.push(cov);
        m.tests.set(name, list);
      }
      byModule.set(r.module, m);
    }

    let runner = 'vitest';
    try {
      const require = createRequire(path.join(this.root, 'package.json'));
      runner = `vitest@${require('vitest/package.json').version}`;
    } catch {
      // keep the bare name
    }
    // Fingerprints (ADR 012), computed by the CLI before Vitest started (in
    // the same process context as the skip-file checks and `fingerprint`;
    // Vitest itself sets variables such as NODE_ENV). Without them the map
    // stays in path mode.
    let fingerprints = {};
    try {
      if (process.env.QUOD_FINGERPRINT_FILE) {
        const { environment: env, invocation: inv } = JSON.parse(
          fs.readFileSync(process.env.QUOD_FINGERPRINT_FILE, 'utf8'),
        );
        fingerprints = {
          environment: env,
          invocation: inv,
          environment_digest: digest(env),
          invocation_digest: digest(inv),
        };
      }
    } catch (e) {
      process.stderr.write(`quod: no environment fingerprint (${e?.message ?? e})\n`);
    }
    const lines = [];
    const emit = (o) => lines.push(JSON.stringify(o));
    emit({
      type: 'header',
      v: CAPTURE_VERSION,
      probe: PROBE_ID,
      capabilities: fingerprints.environment ? ['js.environment'] : [],
      ...fingerprints,
      language: 'javascript',
      ecosystem: 'npm',
      runner,
      node: process.version,
      repo_root: slash(this.root),
    });
    for (const p of [...packages.values()].sort((a, b) => a.name.localeCompare(b.name))) {
      emit({ type: 'package', name: p.name, version: p.version, native: false, dist_hash: p.dist_hash });
    }

    for (const mod of testModules) {
      const file = slash(path.relative(this.root, mod.moduleId));
      const cov = byModule.get(file);
      for (const test of mod.children.allTests()) {
        const name = `${file}::${test.fullName}`;
        const result = test.result();
        const outcome = result.state === 'passed' ? 'passed' : result.state === 'failed' ? 'failed' : 'skipped';
        const diag = test.diagnostic?.();
        const merged = new Map();
        const positions = new Map();
        const files = new Set();
        const pk = new Set();
        let untraced = !cov || cov.concurrent;
        const env = new Map();
        const add = (c) => {
          for (const [k, h] of c.env ?? []) if (!env.has(k)) env.set(k, h);
          for (const [f, ranges] of c.lines) {
            const r = merged.get(f) ?? [];
            r.push(...ranges);
            merged.set(f, r);
          }
          for (const [f, ps] of c.positions) {
            const s = positions.get(f) ?? new Set();
            for (const [l, col] of ps) s.add(`${l}:${col}`);
            positions.set(f, s);
          }
          for (const f of c.files) files.add(f);
          for (const p of c.packages) pk.add(p);
          untraced ||= c.untraced;
        };
        if (cov) {
          for (const s of cov.shared) add(s);
          const own = cov.tests.get(name);
          if (own) own.forEach(add);
          else if (outcome === 'passed') untraced = true; // ran, but no window was recorded
        }
        emit({
          type: 'test',
          name,
          path: file,
          qual: test.fullName,
          params: '',
          outcome,
          duration_ms: Math.round(diag?.duration ?? 0),
          files: [...files].sort(),
          functions: [],
          packages: [...pk].sort(),
          flags: untraced ? ['untraced'] : [],
          env: [...env].sort((a, b) => (a[0] < b[0] ? -1 : 1)),
          lines: [...merged].sort(([a], [b]) => a.localeCompare(b)).map(([f, r]) => [f, normalize(r)]),
          positions: [...positions]
            .sort(([a], [b]) => a.localeCompare(b))
            .map(([f, s]) => [
              f,
              [...s].map((p) => p.split(':').map(Number)).sort((a, b) => a[0] - b[0] || a[1] - b[1]),
            ]),
        });
      }
    }
    emit({ type: 'root', files: this.rootFiles, functions: [], packages: [] });
    fs.mkdirSync(path.dirname(path.resolve(this.out)), { recursive: true });
    fs.writeFileSync(this.out, lines.join('\n') + '\n');
  }
}

function normalize(ranges) {
  const sorted = ranges.map((r) => [...r]).sort((a, b) => a[0] - b[0]);
  const out = [];
  for (const r of sorted) {
    const last = out[out.length - 1];
    if (last && r[0] <= last[1] + 1) last[1] = Math.max(last[1], r[1]);
    else out.push(r);
  }
  return out;
}
