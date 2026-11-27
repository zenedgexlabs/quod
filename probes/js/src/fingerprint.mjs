// Environment and invocation fingerprints for Node (ADR 012, applied to JS).
//
// The environment fingerprint is a flat `key -> value` map of what can change
// test outcomes without going through code the probe observes: the Node
// runtime (its bundled V8, OpenSSL, ICU, ...), the platform, time zone and
// locale, variables read below JavaScript (NODE_*, TZ, LANG, LC_*), and every
// installed package. `quod select` compares it with the map's. Any
// difference outside `dist.*` runs everything (R018); `dist.npm:<name>`
// differences become changed package nodes.
//
// Hashes use the same scheme as the Python probe. Digests are compared only
// between JS-produced fingerprints.

import { createHash } from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const HASH_PREFIX = 'quod v1 env value\0';

export const envKey = (name) => (process.platform === 'win32' ? name.toUpperCase() : name);

export function envValueHash(value) {
  const data = value === undefined || value === null ? 'absent' : `present\0${value}`;
  return createHash('sha256').update(HASH_PREFIX + data, 'utf8').digest('hex');
}

/** Hash of the whole environment (the `env:*` node), minus Quod's own variables. */
export function environHash(env = process.env) {
  const items = Object.keys(env)
    .map((k) => [envKey(k), env[k]])
    .filter(([k]) => !k.startsWith('QUOD_'))
    .sort((a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0));
  return createHash('sha256').update(HASH_PREFIX + JSON.stringify(items), 'utf8').digest('hex');
}

/**
 * The environment as `quod select` sees it, as value hashes: what a test
 * read is recorded against this, not against the worker's environment, which
 * Vitest changes (VITEST, TEST, VITEST_WORKER_ID, NODE_ENV when unset, ...).
 * The in-worker values follow from these, the Vitest version (`dist.*`) and
 * the config (a root file).
 */
export function outerEnv(env = process.env) {
  const values = {};
  for (const k of Object.keys(env)) {
    const key = envKey(k);
    if (!key.startsWith('QUOD_')) values[key] = envValueHash(env[k]);
  }
  return { all: environHash(env), values };
}

/** Canonical digest: keys sorted, compact JSON. */
export function digest(fp) {
  const sorted = Object.fromEntries(Object.keys(fp).sort().map((k) => [k, fp[k]]));
  return createHash('sha256').update(JSON.stringify(sorted), 'utf8').digest('hex');
}

const PROCESS_ENV_PREFIXES = ['NODE_', 'LC_', 'UV_'];
const PROCESS_ENV_NAMES = new Set(['TZ', 'LANG', 'LANGUAGE', 'NODE_OPTIONS']);

function runtime() {
  const out = {
    'interpreter.implementation': 'node',
    'interpreter.version': process.version,
    'interpreter.platform': process.platform,
    'interpreter.arch': process.arch,
    'interpreter.exec_argv': JSON.stringify(process.execArgv),
  };
  for (const [k, v] of Object.entries(process.versions).sort()) {
    if (k !== 'node') out[`interpreter.versions.${k}`] = String(v);
  }
  return out;
}

function system() {
  const safe = (f) => {
    try {
      return String(f());
    } catch (e) {
      return `<error ${e?.name ?? 'Error'}>`;
    }
  };
  return {
    'system.os': process.platform,
    'system.os_release': safe(() => os.release()),
    'system.timezone': safe(() => Intl.DateTimeFormat().resolvedOptions().timeZone),
    'system.timezone_offset': safe(() => new Date(0).getTimezoneOffset()),
    'system.locale': safe(() => Intl.DateTimeFormat().resolvedOptions().locale),
    'system.cpu_count': safe(() => os.availableParallelism?.() ?? os.cpus().length),
    ...(process.env.QUOD_CONTAINER_DIGEST ? { 'system.container': process.env.QUOD_CONTAINER_DIGEST } : {}),
  };
}

function processEnv() {
  const out = {};
  for (const [k, v] of Object.entries(process.env)) {
    const key = envKey(k);
    if (PROCESS_ENV_NAMES.has(key) || PROCESS_ENV_PREFIXES.some((p) => key.startsWith(p))) {
      if (key.startsWith('QUOD_')) continue;
      out[`process_env.${key}`] = envValueHash(v);
    }
  }
  return out;
}

/**
 * Every installed package under `node_modules` directories of `dirs` (and
 * nested ones, and pnpm's `.pnpm` store): `dist.npm:<name>` -> each copy's
 * version and package.json hash. Symlinks are not followed: in pnpm they
 * point into the store (walked directly), in workspaces into repository
 * code (observed as source, not as a package).
 */
export function installedPackages(dirs) {
  const copies = new Map();
  const seen = new Set();
  const visitPackage = (pkgDir) => {
    let text;
    try {
      text = fs.readFileSync(path.join(pkgDir, 'package.json'));
    } catch {
      return;
    }
    let meta = {};
    try {
      meta = JSON.parse(text);
    } catch {
      // keep the hash of the unparseable file
    }
    const name = meta.name || path.basename(pkgDir);
    const h = createHash('sha256').update(text).digest('hex');
    const list = copies.get(name) ?? [];
    list.push(`${meta.version ?? ''} ${h}`);
    copies.set(name, list);
    walkModules(path.join(pkgDir, 'node_modules'));
  };
  const walkModules = (nm) => {
    // Most packages have no nested node_modules: one failed readdir, no
    // realpath (Node's JS realpath stats every path component).
    let entries;
    try {
      entries = fs.readdirSync(nm, { withFileTypes: true });
    } catch {
      return;
    }
    let real;
    try {
      real = fs.realpathSync.native(nm);
    } catch {
      return;
    }
    if (seen.has(real)) return;
    seen.add(real);
    for (const e of entries) {
      if (e.isSymbolicLink()) continue;
      const p = path.join(nm, e.name);
      if (!e.isDirectory()) continue;
      if (e.name === '.pnpm') {
        // Store layout: .pnpm/<name>@<version>/node_modules/<name>
        for (const s of fs.readdirSync(p, { withFileTypes: true })) {
          if (s.isDirectory() && s.name !== 'node_modules') walkModules(path.join(p, s.name, 'node_modules'));
        }
      } else if (e.name.startsWith('@')) {
        for (const s of fs.readdirSync(p, { withFileTypes: true })) {
          if (s.isDirectory() && !s.isSymbolicLink()) visitPackage(path.join(p, s.name));
        }
      } else if (!e.name.startsWith('.')) {
        visitPackage(p);
      }
    }
  };
  for (const d of dirs) walkModules(path.join(d, 'node_modules'));
  const out = {};
  for (const [name, list] of [...copies].sort((a, b) => (a[0] < b[0] ? -1 : 1))) {
    out[`dist.npm:${name}`] = [...new Set(list)].sort().join(';');
  }
  return out;
}

/** The environment fingerprint for tests run from `dir` in repository `root`. */
export function environment(root, dir = root) {
  // Node resolves packages from the importing file's directory upwards, so
  // every ancestor's node_modules can supply them.
  const dirs = new Set([path.resolve(root)]);
  for (let d = path.resolve(dir); ; d = path.dirname(d)) {
    dirs.add(d);
    if (path.dirname(d) === d) break;
  }
  return { ...runtime(), ...system(), ...processEnv(), ...installedPackages(dirs) };
}

/**
 * The invocation fingerprint: the runner, its arguments (Quod's own
 * excluded) and the test directory relative to the repository root, which
 * is what an agent needs to run the same invocation again.
 */
export function invocation(args, { runner = 'vitest', root, dir } = {}) {
  const fp = {
    'invocation.runner': runner,
    'invocation.args': JSON.stringify(args),
    'invocation.env.NODE_OPTIONS': envValueHash(process.env.NODE_OPTIONS),
  };
  if (root && dir) fp['invocation.dir'] = path.relative(root, dir).split(path.sep).join('/') || '.';
  return fp;
}
