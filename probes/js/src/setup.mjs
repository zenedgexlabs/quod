// Skipcert Vitest worker setup (ADR 013). Must be the *first* setup file.
//
// Record mode (SKIPCERT_RECORD_DIR set): collects, per test, the functions
// that ran (V8 precise coverage) as original start positions, plus the files
// whose module-level code ran. One JSON file per test module is written for
// the reporter to merge.
//
// Attribution, which is what makes skips sound:
// - a test's window runs from its first `beforeEach` to its last `afterEach`;
// - code between two windows (late `afterEach` hooks of the previous test,
//   early `beforeEach` hooks of the next) is attributed to *both* tests;
// - module evaluation, `beforeAll` and `afterAll` (outside every window) are
//   attributed to every test of the module;
// - recording runs one test at a time (maxConcurrency 1); if tests still
//   overlap (no serialising wrapper), they are flagged `untraced`.
//
// Skip mode (SKIPCERT_SKIP_FILE set): tests named in the file are skipped
// before their own hooks run.

import { afterAll, afterEach, beforeEach, vi } from 'vitest';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { executedFunctions } from './coverage.mjs';
import { envKey, envValueHash, environHash } from './fingerprint.mjs';

const ROOT = path.resolve(process.env.SKIPCERT_ROOT || process.cwd());
const RECORD_DIR = process.env.SKIPCERT_RECORD_DIR;
const SKIP_FILE = process.env.SKIPCERT_SKIP_FILE;

const slash = (p) => p.split(path.sep).join('/');
const rel = (abs) => slash(path.relative(ROOT, abs));

/** Suite path and test name joined with ` > `, as Vitest reports `fullName`. */
export function fullName(task) {
  const parts = [];
  for (let t = task; t && t !== task.file && t.type !== 'module'; t = t.suite) {
    if (t.name && !(t.filepath && t === t.file)) parts.unshift(t.name);
  }
  return parts.join(' > ');
}

/** `path::suite > test`: the name the Rust side derives stable IDs from. */
export function testName(task) {
  return `${rel(task.file.filepath)}::${fullName(task)}`;
}

// --- skip mode -------------------------------------------------------------

if (SKIP_FILE) {
  const skip = new Set(
    fs
      .readFileSync(SKIP_FILE, 'utf8')
      .split(/\r?\n/)
      .map((l) => l.trim())
      .filter((l) => l && !l.startsWith('#')),
  );
  beforeEach((ctx) => {
    if (skip.has(testName(ctx.task))) ctx.skip();
  });
}

// --- record mode -----------------------------------------------------------

const g = globalThis;

async function session() {
  if (g.__skipcert) return g.__skipcert;
  const inspector = await import('node:inspector/promises');
  const s = new inspector.Session();
  s.connect();
  await s.post('Profiler.enable');
  await s.post('Debugger.enable');
  await s.post('Profiler.startPreciseCoverage', { callCount: true, detailed: false });
  g.__skipcert = { s, sources: new Map(), packages: new Map(), lineCounts: new Map(), env: new Map() };
  trackEnv(g.__skipcert);
  return g.__skipcert;
}

function packageOf(abs) {
  const parts = slash(abs).split('/');
  const i = parts.lastIndexOf('node_modules');
  if (i < 0) return null;
  const name = parts[i + 1]?.startsWith('@') ? `${parts[i + 1]}/${parts[i + 2]}` : parts[i + 1];
  if (!name || name === '.vite' || name === '.cache') return null;
  const dir = parts.slice(0, i + 1 + name.split('/').length).join('/');
  return { name, dir };
}

function packageInfo(sc, pkg) {
  if (!sc.packages.has(pkg.name)) {
    let version = '';
    let hash = '';
    try {
      const text = fs.readFileSync(path.join(pkg.dir, 'package.json'));
      version = JSON.parse(text).version || '';
      hash = createHash('sha256').update(text).digest('hex');
    } catch {
      // unreadable: the identity stays empty
    }
    sc.packages.set(pkg.name, { name: pkg.name, version, dist_hash: hash });
  }
  return sc.packages.get(pkg.name);
}

function moduleFor(abs) {
  const em = g.__vitest_worker__?.evaluatedModules;
  const nodes = em?.fileToModulesMap?.get(slash(abs));
  if (!nodes) return null;
  for (const node of nodes) {
    if (node.meta?.code != null) {
      let map = null;
      try {
        map = em.getModuleSourceMapById?.(node.id) ?? null;
      } catch {
        map = null;
      }
      return { code: node.meta.code, map: map?.map ?? map };
    }
  }
  return null;
}

const empty = () => ({
  positions: new Map(),
  files: new Set(),
  whole: new Map(),
  packages: new Set(),
  env: new Map(),
  untraced: false,
});

/**
 * Records reads of `process.env` as `env:NAME` observations (value hashes),
 * into the same windows as coverage. Enumerating the environment
 * (`Object.keys`, spreading) depends on all of it: `env:*`. Keys the process
 * wrote itself (`vi.stubEnv`, fixtures) are in-process state, not inputs.
 */
function trackEnv(sc) {
  const target = process.env;
  const written = new Set();
  const note = (k, h) => {
    if (!k.startsWith('SKIPCERT_') && !sc.env.has(k)) sc.env.set(k, h);
  };
  process.env = new Proxy(target, {
    get(t, k) {
      const v = t[k];
      if (typeof k === 'string' && !written.has(envKey(k))) note(envKey(k), envValueHash(v));
      return v;
    },
    has(t, k) {
      const r = k in t;
      if (typeof k === 'string' && !written.has(envKey(k))) note(envKey(k), envValueHash(r ? t[k] : undefined));
      return r;
    },
    set(t, k, v) {
      if (typeof k === 'string') written.add(envKey(k));
      t[k] = v;
      return true;
    },
    deleteProperty(t, k) {
      if (typeof k === 'string') written.add(envKey(k));
      delete t[k];
      return true;
    },
    ownKeys(t) {
      note('*', environHash(t));
      return Reflect.ownKeys(t);
    },
  });
  // vi.stubEnv reads the old value only to restore it later.
  try {
    const orig = vi.stubEnv;
    vi.stubEnv = function (name, ...rest) {
      if (typeof name === 'string') written.add(envKey(name));
      return orig.call(this, name, ...rest);
    };
  } catch {
    // a frozen `vi`: stubbed reads stay recorded (conservative)
  }
}

/** Coverage since the last take. */
async function take() {
  const sc = await session();
  const { result } = await sc.s.post('Profiler.takePreciseCoverage');
  const out = empty();
  out.env = sc.env;
  sc.env = new Map();
  for (const script of result) {
    if (!script.url.startsWith('file:')) continue;
    if (!script.functions.some((f) => f.ranges[0]?.count > 0)) continue;
    let abs;
    try {
      abs = fileURLToPath(script.url);
    } catch {
      continue;
    }
    const pkg = packageOf(abs);
    if (pkg) {
      out.packages.add(packageInfo(sc, pkg).name);
      continue;
    }
    if (!slash(abs).startsWith(slash(ROOT) + '/')) continue;
    const file = rel(abs);
    // Over-approximation when positions can't be mapped: every line ran.
    const whole = () => {
      if (!sc.lineCounts.has(file)) {
        let n = 1;
        try {
          n = fs.readFileSync(abs, 'utf8').split('\n').length;
        } catch {
          // keep 1
        }
        sc.lineCounts.set(file, n);
      }
      out.whole.set(file, sc.lineCounts.get(file));
    };
    let source = sc.sources.get(script.scriptId);
    if (source === undefined) {
      try {
        source = (await sc.s.post('Debugger.getScriptSource', { scriptId: script.scriptId })).scriptSource;
      } catch {
        source = null;
      }
      sc.sources.set(script.scriptId, source);
    }
    if (source == null) {
      whole();
      continue;
    }
    const mod = moduleFor(abs);
    let code = null;
    let map = null;
    if (mod) {
      code = mod.code;
      map = mod.map;
      if (map) {
        const base = path.basename(abs);
        const idx = (map.sources || []).findIndex((s) => s && path.basename(s) === base);
        if (idx !== 0) {
          whole();
          continue;
        }
      }
    } else {
      // Not from the module runner (loaded by Node directly): positions are
      // only meaningful if V8 ran the file unmodified.
      let original = null;
      try {
        original = fs.readFileSync(abs, 'utf8');
      } catch {
        // generated at runtime
      }
      if (original === null) continue;
      if (source !== original) {
        whole();
        continue;
      }
    }
    const res = executedFunctions({ functions: script.functions, scriptSource: source, code, map });
    if (res.all) {
      whole();
      continue;
    }
    if (res.module) out.files.add(file);
    if (res.starts.length) {
      const set = out.positions.get(file) ?? new Set();
      for (const [l, c] of res.starts) set.add(`${l}:${c}`);
      out.positions.set(file, set);
    }
  }
  return out;
}

function merge(into, cov) {
  for (const [file, set] of cov.positions) {
    const s = into.positions.get(file) ?? new Set();
    for (const p of set) s.add(p);
    into.positions.set(file, s);
  }
  for (const f of cov.files) into.files.add(f);
  for (const [f, n] of cov.whole) into.whole.set(f, n);
  for (const p of cov.packages) into.packages.add(p);
  for (const [k, h] of cov.env) if (!into.env.has(k)) into.env.set(k, h);
  into.untraced ||= cov.untraced;
}

function encode(c) {
  return {
    positions: [...c.positions].map(([f, set]) => [
      f,
      [...set].map((p) => p.split(':').map(Number)).sort((a, b) => a[0] - b[0] || a[1] - b[1]),
    ]),
    files: [...c.files].sort(),
    lines: [...c.whole].map(([f, n]) => [f, [[1, n]]]),
    packages: [...c.packages].sort(),
    env: [...c.env].sort((a, b) => (a[0] < b[0] ? -1 : 1)),
    untraced: c.untraced,
  };
}

if (RECORD_DIR) {
  await session();
  // Everything before this module's first test (worker start, other setup
  // files, module evaluation, beforeAll) belongs to every test of the module.
  const shared = empty();
  const own = new Map();
  let prev = null;
  let concurrent = false;
  let moduleFile = null;
  merge(shared, await take());

  beforeEach(async (ctx) => {
    moduleFile ??= ctx.task.file.filepath;
    // Concurrent tests share windows, unless recording serialised them.
    if (ctx.task.concurrent && process.env.SKIPCERT_SERIAL !== '1') concurrent = true;
    const name = testName(ctx.task);
    const cov = await take();
    if (!own.has(name)) own.set(name, empty());
    if (prev === null) merge(shared, cov);
    else {
      merge(own.get(prev), cov);
      merge(own.get(name), cov);
    }
  });

  afterEach(async (ctx) => {
    const name = testName(ctx.task);
    if (!own.has(name)) own.set(name, empty());
    merge(own.get(name), await take());
    prev = name;
  });

  afterAll(async () => {
    merge(shared, await take());
    const record = {
      module: moduleFile ? rel(moduleFile) : null,
      concurrent,
      shared: encode(shared),
      tests: Object.fromEntries([...own].map(([n, c]) => [n, encode(c)])),
      packages: [...g.__skipcert.packages.values()],
    };
    fs.mkdirSync(RECORD_DIR, { recursive: true });
    const id = createHash('sha256')
      .update(`${record.module}|${process.pid}|${Date.now()}|${Math.random()}`)
      .digest('hex')
      .slice(0, 16);
    fs.writeFileSync(path.join(RECORD_DIR, `${id}.json`), JSON.stringify(record));
  });
}
