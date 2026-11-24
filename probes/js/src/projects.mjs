// Vitest projects (`test.projects`) don't inherit the root's setup files,
// so each project's config is wrapped too: the probe's setup file must be
// the first setup file of every project, or its tests would run unobserved
// (and come out `untraced`, so nothing could be skipped).

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

const CONFIG_NAMES = ['vitest.config', 'vite.config'];
const CONFIG_EXTS = ['.ts', '.mts', '.cts', '.js', '.mjs', '.cjs'];

function configIn(dir) {
  for (const n of CONFIG_NAMES) {
    for (const e of CONFIG_EXTS) {
      const p = path.join(dir, n + e);
      if (fs.existsSync(p)) return p;
    }
  }
  return null;
}

/** Expands a project glob: `*` and `**` segments (what project lists use). */
function expand(root, pattern) {
  const neg = pattern.startsWith('!');
  if (neg) return [];
  const parts = pattern.replace(/\\/g, '/').split('/').filter((p) => p && p !== '.');
  let current = [path.resolve(root)];
  for (const part of parts) {
    const next = [];
    for (const base of current) {
      if (part === '**') {
        const walk = (d) => {
          next.push(d);
          for (const e of fs.readdirSync(d, { withFileTypes: true })) {
            if (e.isDirectory() && e.name !== 'node_modules' && !e.name.startsWith('.')) walk(path.join(d, e.name));
          }
        };
        walk(base);
      } else if (part.includes('*')) {
        const re = new RegExp('^' + part.replace(/[.+^${}()|[\]]/g, '\\$&').replace(/\*/g, '.*') + '$');
        if (!fs.existsSync(base) || !fs.statSync(base).isDirectory()) continue;
        for (const e of fs.readdirSync(base)) if (re.test(e)) next.push(path.join(base, e));
      } else {
        const p = path.join(base, part);
        if (fs.existsSync(p)) next.push(p);
      }
    }
    current = next;
  }
  return current;
}

/** The name Vitest would give the project: its package name, else its directory. */
function projectName(dir) {
  try {
    const name = JSON.parse(fs.readFileSync(path.join(dir, 'package.json'), 'utf8')).name;
    if (name) return name;
  } catch {
    // no package.json
  }
  return path.basename(dir);
}

export function projectWrapperSource({ dir, userConfig, setup, cacheDir }) {
  const imp = userConfig
    ? `import userExport from ${JSON.stringify(pathToFileURL(userConfig).href)};`
    : 'const userExport = {};';
  return `${imp}
import { mergeConfig } from 'vitest/config';
import { wrapProjects } from ${JSON.stringify(import.meta.url)};

export default async (env) => {
  const user = typeof userExport === 'function' ? await userExport(env) : await userExport;
  const config = mergeConfig(user ?? {}, { root: ${JSON.stringify(dir)} });
  config.test ??= {};
  // The wrapper lives elsewhere, so name the project as Vitest would have.
  config.test.name ??= ${JSON.stringify(projectName(dir))};
  const userSetup = [].concat(config.test.setupFiles ?? []);
  config.test.setupFiles = [${JSON.stringify(setup)}, ...userSetup.filter((s) => s !== ${JSON.stringify(setup)})];
  if (process.env.SKIPCERT_RECORD_DIR) {
    // Recording runs one test at a time, so every test gets its own coverage
    // window. Concurrency doesn't change what code a test executes; skip
    // runs keep the project's own concurrency.
    config.test.maxConcurrency = 1;
    config.test.sequence = { ...(config.test.sequence ?? {}), concurrent: false };
    process.env.SKIPCERT_SERIAL = '1';
  }
  // Projects can nest.
  if (Array.isArray(config.test.projects)) {
    config.test.projects = wrapProjects(config.test.projects, {
      root: ${JSON.stringify(dir)},
      setup: ${JSON.stringify(setup)},
      cacheDir: ${JSON.stringify(cacheDir)},
    });
  }
  return config;
};
`;
}

/** Record-mode settings for an inline project config object (see the wrappers). */
function serialise(test) {
  if (!process.env.SKIPCERT_RECORD_DIR) return test;
  return { ...test, maxConcurrency: 1, sequence: { ...(test.sequence ?? {}), concurrent: false } };
}

/**
 * Rewrites `projects` so every project starts with the probe's setup file.
 * String entries (globs, directories, config files) become generated wrapper
 * configs in `cacheDir`; inline project objects get the setup file prepended.
 */
let generatedCount = 0;

export function wrapProjects(projects, { root, setup, cacheDir }) {
  const out = [];
  for (const entry of projects) {
    if (typeof entry === 'string') {
      for (const match of expand(root, entry)) {
        const isDir = fs.statSync(match).isDirectory();
        const dir = isDir ? match : path.dirname(match);
        const userConfig = isDir ? configIn(match) : match;
        if (isDir && !userConfig && !fs.existsSync(path.join(match, 'package.json'))) continue;
        // Vitest resolves a project's root and test globs from its config
        // file's directory, so the wrapper sits next to the project's own
        // config. The CLI deletes it after the run (see `generated`).
        const file = path.join(dir, `vitest.skipcert-${process.pid}-${generatedCount++}.config.mjs`);
        fs.writeFileSync(file, projectWrapperSource({ dir, userConfig, setup, cacheDir }));
        fs.appendFileSync(path.join(cacheDir, 'generated.txt'), `${file}${os.EOL}`);
        out.push(file);
      }
    } else if (entry && typeof entry === 'object') {
      const copy = { ...entry, test: serialise({ ...(entry.test ?? {}) }) };
      const userSetup = [].concat(copy.test.setupFiles ?? []);
      copy.test.setupFiles = [setup, ...userSetup.filter((s) => s !== setup)];
      out.push(copy);
    } else {
      out.push(entry);
    }
  }
  return out;
}
