// Capture and results writers shared by the Vitest and Jest reporters.
//
// Both reporters reduce their framework's results to a list of tests:
// `{ file, name, qual, outcome: 'passed'|'failed'|'skipped', duration_ms,
// key? }`, where `name` is `file::suite > test`, the name the Rust side
// derives stable IDs from, and `key` (if present) is the name the workers
// keyed coverage windows by; `key: null` means the test can't be
// attributed (untraced).

import fs from 'node:fs';
import path from 'node:path';
import { CAPTURE_VERSION, PROBE_ID } from './version.mjs';
import { digest } from './fingerprint.mjs';

const slash = (p) => p.split(path.sep).join('/');

/**
 * Per-test outcomes for `quod verdict` (QUOD_RESULTS_OUT). Tests Quod skipped
 * (the skip file) are not results: they carry certificates instead.
 */
export function writeResults(tests) {
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
  const lines = tests
    .filter((t) => !certified.has(t.name))
    .map((t) =>
      JSON.stringify({
        duration_ms: Math.round((t.duration_ms ?? 0) * 1000) / 1000,
        name: t.name,
        outcome: t.outcome,
        type: 'result',
      }),
    )
    .sort();
  fs.writeFileSync(out, lines.length ? `${lines.join('\n')}\n` : '');
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

/** Reads the workers' module records from `dir`, unioned per module. */
function readRecords(dir) {
  const records = [];
  for (const f of fs.existsSync(dir) ? fs.readdirSync(dir) : []) {
    if (f.endsWith('.json') && f !== 'fingerprint.json') records.push(JSON.parse(fs.readFileSync(path.join(dir, f), 'utf8')));
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
  return { byModule, packages };
}

function fingerprints() {
  // Computed by the CLI before the runner started (in the same process
  // context as the skip-file checks and `fingerprint`; runners set variables
  // such as NODE_ENV). Without them the map stays in path mode.
  try {
    if (process.env.QUOD_FINGERPRINT_FILE) {
      const { environment: env, invocation: inv } = JSON.parse(fs.readFileSync(process.env.QUOD_FINGERPRINT_FILE, 'utf8'));
      return { environment: env, invocation: inv, environment_digest: digest(env), invocation_digest: digest(inv) };
    }
  } catch (e) {
    process.stderr.write(`quod: no environment fingerprint (${e?.message ?? e})\n`);
  }
  return {};
}

/**
 * Writes the capture file (protocol v1) from the workers' records and the
 * runner's test list.
 */
export function writeCapture({ root, dir, out, rootFiles, runner, tests }) {
  const { byModule, packages } = readRecords(dir);
  const fp = fingerprints();
  const lines = [];
  const emit = (o) => lines.push(JSON.stringify(o));
  emit({
    type: 'header',
    v: CAPTURE_VERSION,
    probe: PROBE_ID,
    capabilities: fp.environment ? ['js.environment'] : [],
    ...fp,
    language: 'javascript',
    ecosystem: 'npm',
    runner,
    node: process.version,
    repo_root: slash(root),
  });
  for (const p of [...packages.values()].sort((a, b) => a.name.localeCompare(b.name))) {
    emit({ type: 'package', name: p.name, version: p.version, native: false, dist_hash: p.dist_hash });
  }
  for (const test of tests) {
    const cov = byModule.get(test.file);
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
      const key = test.key === undefined ? test.name : test.key;
      const own = key === null ? undefined : cov.tests.get(key);
      if (key === null) untraced = true;
      else if (own) own.forEach(add);
      else if (test.outcome === 'passed') untraced = true; // ran, but no window was recorded
    }
    emit({
      type: 'test',
      name: test.name,
      path: test.file,
      qual: test.qual,
      params: '',
      outcome: test.outcome,
      duration_ms: Math.round(test.duration_ms ?? 0),
      files: [...files].sort(),
      functions: [],
      packages: [...pk].sort(),
      flags: untraced ? ['untraced'] : [],
      env: [...env].sort((a, b) => (a[0] < b[0] ? -1 : 1)),
      lines: [...merged].sort(([a], [b]) => a.localeCompare(b)).map(([f, r]) => [f, normalize(r)]),
      positions: [...positions]
        .sort(([a], [b]) => a.localeCompare(b))
        .map(([f, s]) => [f, [...s].map((p) => p.split(':').map(Number)).sort((a, b) => a[0] - b[0] || a[1] - b[1])]),
    });
  }
  emit({ type: 'root', files: rootFiles, functions: [], packages: [] });
  fs.mkdirSync(path.dirname(path.resolve(out)), { recursive: true });
  fs.writeFileSync(out, lines.join('\n') + '\n');
}
