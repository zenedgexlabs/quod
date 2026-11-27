// Framework-neutral per-test recording (ADR 013), shared by the Vitest and
// Jest adapters. Must not use top-level await: Jest's setup file loads it
// with a native `require()`.
//
// Record mode collects, per test, the functions that ran (V8 precise
// coverage) as original start positions, plus the files whose module-level
// code ran. One JSON file per test module is written for the reporter to
// merge.
//
// Attribution, which is what makes skips sound:
// - a test's window runs from its first `beforeEach` to its last `afterEach`;
// - code between two windows (late `afterEach` hooks of the previous test,
//   early `beforeEach` hooks of the next) is attributed to *both* tests;
// - module evaluation, `beforeAll` and `afterAll` (outside every window) are
//   attributed to every test of the module;
// - recording runs one test at a time; tests that still overlap are flagged
//   `untraced`;
// - when test files share modules (Vitest `isolate: false`), a module's top
//   level runs once per worker, in the first importer's window. Every later
//   test module of the worker is then attributed every module evaluated in
//   the worker so far (ADR 019). Isolated runs re-evaluate per test file.

import { createHash } from 'node:crypto';
import inspector from 'node:inspector';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { executedFunctions } from './coverage.mjs';
import { envKey, envValueHash, environHash } from './fingerprint.mjs';

const slash = (p) => p.split(path.sep).join('/');

/** The CLI's environment (`outerEnv`), or null when the runner was started directly. */
function readOuterEnv() {
  try {
    const f = process.env.QUOD_FINGERPRINT_FILE;
    return f ? (JSON.parse(fs.readFileSync(f, 'utf8')).env ?? null) : null;
  } catch {
    return null;
  }
}

/** Test names listed in the skip file (`quod select --skip-out`). */
export function readSkipFile(file) {
  return new Set(
    fs
      .readFileSync(file, 'utf8')
      .split(/\r?\n/)
      .map((l) => l.trim())
      .filter((l) => l && !l.startsWith('#')),
  );
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

const INLINE_MAP = /\/\/[#@] sourceMappingURL=data:application\/json[^,]*;base64,([A-Za-z0-9+/=]+)\s*$/m;
// Jest's module wrapper around transformed code.
const JEST_WRAPPER = /^\(\{"Object\.<anonymous>":function\([^)]*\)\{/;

/**
 * Generated code and source map from the script itself: a transform that
 * left an inline source map (Jest, ts-node, babel-register). Returns null if
 * there is none.
 */
export function inlineModule(source) {
  const m = INLINE_MAP.exec(source);
  if (!m) return null;
  let map;
  try {
    map = JSON.parse(Buffer.from(m[1], 'base64').toString('utf8'));
  } catch {
    return null;
  }
  let code = source;
  const w = JEST_WRAPPER.exec(code);
  if (w) code = code.slice(w[0].length);
  return { code, map };
}

const empty = () => ({
  positions: new Map(),
  files: new Set(),
  whole: new Map(),
  packages: new Set(),
  env: new Map(),
  untraced: false,
});

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

/**
 * Records reads of `proc.env` as `env:NAME` observations (value hashes),
 * into the same windows as coverage. Enumerating the environment
 * (`Object.keys`, spreading) depends on all of it: `env:*`. Keys the process
 * wrote itself (stubs, fixtures) are in-process state, not inputs.
 *
 * Values are recorded as the CLI saw them before starting the runner
 * (`outerEnv`), the environment `quod select` compares with. Returns a
 * function that marks a key written (for the runner's env stubs).
 */
function trackEnv(sc, proc) {
  const target = proc.env;
  const written = new Set();
  const outer = readOuterEnv();
  const note = (k, h) => {
    if (k.startsWith('QUOD_') || sc.env.has(k)) return;
    if (outer) h = k === '*' ? outer.all : (outer.values[k] ?? envValueHash(undefined));
    sc.env.set(k, h);
  };
  proc.env = new Proxy(target, {
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
  return (name) => {
    if (typeof name === 'string') written.add(envKey(name));
  };
}

/**
 * A per-worker recorder.
 *
 * - `root`: repository root; files outside it are ignored.
 * - `recordDir`: where module records go.
 * - `proc`: the `process` object tests see (Jest gives each test file its
 *   own copy).
 * - `moduleFor(abs)`: optional `{ code, map }` from the runner's module
 *   system (Vite); otherwise inline source maps are used.
 * - `store`: an object to keep the inspector session on (one per worker).
 * - `sharedModules`: test files share one module cache (no per-file
 *   isolation), so module evaluation is attributed worker-wide.
 */
export function createRecorder({
  root,
  recordDir,
  proc = process,
  moduleFor = () => null,
  store = globalThis,
  sharedModules = false,
}) {
  const rel = (abs) => slash(path.relative(root, abs));
  let sc = null;
  let markWritten = () => {};

  /**
   * Starts coverage synchronously (the in-process inspector answers
   * synchronously), so a setup file can start it before the runner
   * evaluates the test module.
   */
  function session() {
    if (store.__quod) {
      sc = store.__quod;
    } else {
      const raw = new inspector.Session();
      raw.connect();
      const post = (method, params = {}) =>
        new Promise((resolve, reject) => raw.post(method, params, (e, r) => (e ? reject(e) : resolve(r))));
      for (const [m, p] of [
        ['Profiler.enable', {}],
        ['Debugger.enable', {}],
        ['Profiler.startPreciseCoverage', { callCount: true, detailed: false }],
      ]) {
        raw.post(m, p, (e) => {
          if (e) throw e;
        });
      }
      sc = store.__quod = {
        s: { post },
        sources: new Map(),
        packages: new Map(),
        lineCounts: new Map(),
        env: new Map(),
        // Files whose module-level code ran in this worker.
        evaluated: new Set(),
      };
    }
    if (!proc.__quodEnv) {
      proc.__quodEnv = trackEnv(sc, proc);
    }
    markWritten = proc.__quodEnv;
    return sc;
  }

  /** Coverage since the last take. */
  async function take() {
    session();
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
      // path.relative, not a prefix test: Windows paths are case-insensitive
      // (the drive letter's case differs between V8's URLs and a cwd).
      const file = rel(abs);
      if (file === '' || file.startsWith('../') || path.isAbsolute(file)) continue;
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
        sc.evaluated.add(file);
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
      const mod = moduleFor(abs) ?? inlineModule(source);
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
        // Loaded unmodified by Node: positions are only meaningful if V8 ran
        // the file as it is on disk.
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
      if (res.module) {
        out.files.add(file);
        sc.evaluated.add(file);
      }
      if (res.starts.length) {
        const set = out.positions.get(file) ?? new Set();
        for (const [l, c] of res.starts) set.add(`${l}:${c}`);
        out.positions.set(file, set);
      }
    }
    return out;
  }

  /**
   * Starts recording one test module. Returns the hook bodies the adapter
   * registers: `before(name, { concurrent, moduleFile })`, `after(name)`,
   * `finish()`. A `name` of null means the test can't be identified, which
   * makes the module's shared coverage untraced.
   */
  async function module() {
    session();
    // Everything before this module's first test (worker start, other setup
    // files, module evaluation, beforeAll) belongs to every test of the module.
    const shared = empty();
    const own = new Map();
    let prev = null;
    let concurrent = false;
    let moduleFile = null;
    merge(shared, await take());
    return {
      markWritten: (name) => markWritten(name),
      async before(name, info = {}) {
        moduleFile ??= info.moduleFile ?? null;
        if (info.concurrent) concurrent = true;
        const cov = await take();
        if (name === null) {
          shared.untraced = true;
          return;
        }
        if (!own.has(name)) own.set(name, empty());
        if (prev === null) merge(shared, cov);
        else {
          merge(own.get(prev), cov);
          merge(own.get(name), cov);
        }
      },
      async after(name) {
        const cov = await take();
        if (name === null) {
          merge(shared, cov);
          shared.untraced = true;
          return;
        }
        if (!own.has(name)) own.set(name, empty());
        merge(own.get(name), cov);
        prev = name;
      },
      async finish() {
        merge(shared, await take());
        if (sharedModules) for (const f of sc.evaluated) shared.files.add(f);
        const record = {
          module: moduleFile ? rel(moduleFile) : null,
          concurrent,
          shared: encode(shared),
          tests: Object.fromEntries([...own].map(([n, c]) => [n, encode(c)])),
          packages: [...sc.packages.values()],
        };
        fs.mkdirSync(recordDir, { recursive: true });
        const id = createHash('sha256')
          .update(`${record.module}|${process.pid}|${Date.now()}|${Math.random()}`)
          .digest('hex')
          .slice(0, 16);
        fs.writeFileSync(path.join(recordDir, `${id}.json`), JSON.stringify(record));
      },
    };
  }

  return { module, rel, start: session };
}
