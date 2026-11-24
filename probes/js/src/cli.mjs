#!/usr/bin/env node
// skipcert-vitest: runs Vitest with the Skipcert probe.
//
//   skipcert-vitest record --out capture.jsonl [--root DIR] [--dir DIR] [-- <vitest args>]
//   skipcert-vitest run --skip skip.txt [--root DIR] [--dir DIR] [-- <vitest args>]
//
// --root is the repository root (paths in the capture are relative to it);
// --dir is where Vitest runs (a workspace package), default --root.
//
// The user's Vitest config is wrapped, not modified: a generated config
// (under node_modules/.cache/skipcert) imports it, puts the probe's setup
// file first and adds the reporter. The repository's working tree is left
// untouched, so the wrapper can't show up as a change.

import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import { createRequire } from 'node:module';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const CONFIG_NAMES = ['vitest.config', 'vite.config'];
const CONFIG_EXTS = ['.ts', '.mts', '.cts', '.js', '.mjs', '.cjs'];

function usage(msg) {
  if (msg) console.error(`skipcert-vitest: ${msg}`);
  console.error('usage: skipcert-vitest record --out FILE [--root DIR] [-- vitest args]');
  console.error('       skipcert-vitest run --skip FILE [--root DIR] [-- vitest args]');
  process.exit(2);
}

export function parseArgs(argv) {
  const [cmd, ...rest] = argv;
  if (cmd !== 'record' && cmd !== 'run') usage(`unknown command ${cmd ?? ''}`);
  const opts = { cmd, root: process.cwd(), dir: null, out: null, skip: null, passthrough: [] };
  for (let i = 0; i < rest.length; i++) {
    const a = rest[i];
    if (a === '--') {
      opts.passthrough = rest.slice(i + 1);
      break;
    } else if (a === '--out') opts.out = rest[++i];
    else if (a === '--skip') opts.skip = rest[++i];
    else if (a === '--root') opts.root = rest[++i];
    else if (a === '--dir') opts.dir = rest[++i];
    else usage(`unknown option ${a}`);
  }
  if (cmd === 'record' && !opts.out) usage('record needs --out');
  if (cmd === 'run' && !opts.skip) usage('run needs --skip');
  opts.root = path.resolve(opts.root);
  opts.dir = path.resolve(opts.root, opts.dir ?? '.');
  return opts;
}

function findUserConfig(root, passthrough) {
  const i = passthrough.findIndex((a) => a === '--config' || a === '-c' || a.startsWith('--config='));
  if (i >= 0) {
    const a = passthrough[i];
    const value = a.startsWith('--config=') ? a.slice('--config='.length) : passthrough[i + 1];
    passthrough.splice(i, a.startsWith('--config=') ? 1 : 2);
    return path.resolve(root, value);
  }
  for (const n of CONFIG_NAMES) {
    for (const e of CONFIG_EXTS) {
      const p = path.join(root, n + e);
      if (fs.existsSync(p)) return p;
    }
  }
  return null;
}

export function wrapperSource({ root, userConfig, setup, reporter, cacheDir }) {
  const imp = userConfig
    ? `import userExport from ${JSON.stringify(pathToFileURL(userConfig).href)};`
    : 'const userExport = {};';
  return `${imp}
import { mergeConfig } from 'vitest/config';
import { wrapProjects } from ${JSON.stringify(pathToFileURL(path.join(HERE, 'projects.mjs')).href)};

export default async (env) => {
  const user = typeof userExport === 'function' ? await userExport(env) : await userExport;
  const config = mergeConfig(user ?? {}, { root: ${JSON.stringify(root)} });
  config.test ??= {};
  const userSetup = [].concat(config.test.setupFiles ?? []);
  // The probe's setup file must come first: its hooks bound each test's window.
  config.test.setupFiles = [${JSON.stringify(setup)}, ...userSetup.filter((s) => s !== ${JSON.stringify(setup)})];
  if (process.env.SKIPCERT_RECORD_DIR) {
    // Recording runs one test at a time, so every test gets its own coverage
    // window. Concurrency doesn't change what code a test executes; skip
    // runs keep the project's own concurrency.
    config.test.maxConcurrency = 1;
    config.test.sequence = { ...(config.test.sequence ?? {}), concurrent: false };
    process.env.SKIPCERT_SERIAL = '1';
  }
  if (Array.isArray(config.test.projects)) {
    config.test.projects = wrapProjects(config.test.projects, {
      root: ${JSON.stringify(root)},
      setup: ${JSON.stringify(setup)},
      cacheDir: ${JSON.stringify(cacheDir ?? root)},
    });
  }
  const reporters = [].concat(config.test.reporters ?? ['default']);
  config.test.reporters = [...reporters, ${JSON.stringify(reporter)}];
  return config;
};
`;
}

function vitestBin(root) {
  const require = createRequire(path.join(root, 'package.json'));
  const pkgPath = require.resolve('vitest/package.json');
  const pkg = JSON.parse(fs.readFileSync(pkgPath, 'utf8'));
  const bin = typeof pkg.bin === 'string' ? pkg.bin : pkg.bin.vitest;
  return path.join(path.dirname(pkgPath), bin);
}

export function main(argv) {
  const opts = parseArgs(argv);
  const userConfig = findUserConfig(opts.dir, opts.passthrough);
  const cache = path.join(opts.dir, 'node_modules', '.cache', 'skipcert');
  fs.mkdirSync(cache, { recursive: true });
  const wrapper = path.join(cache, 'vitest.config.mjs');
  fs.writeFileSync(
    wrapper,
    wrapperSource({
      root: opts.dir,
      userConfig,
      setup: path.join(HERE, 'setup.mjs'),
      reporter: path.join(HERE, 'reporter.mjs'),
      cacheDir: cache,
    }),
  );
  const env = { ...process.env, SKIPCERT_ROOT: opts.root };
  let recordDir = null;
  if (opts.cmd === 'record') {
    recordDir = fs.mkdtempSync(path.join(os.tmpdir(), 'skipcert-vitest-'));
    env.SKIPCERT_RECORD_DIR = recordDir;
    env.SKIPCERT_RECORD_OUT = path.resolve(opts.out);
    env.SKIPCERT_ROOT_FILES = JSON.stringify(
      userConfig ? [path.relative(opts.root, userConfig).split(path.sep).join('/')] : [],
    );
  } else {
    env.SKIPCERT_SKIP_FILE = path.resolve(opts.skip);
  }
  const res = spawnSync(process.execPath, [vitestBin(opts.dir), 'run', '--config', wrapper, ...opts.passthrough], {
    cwd: opts.dir,
    env,
    stdio: 'inherit',
  });
  if (recordDir) fs.rmSync(recordDir, { recursive: true, force: true });
  // Project wrappers generated next to project configs (projects.mjs).
  const generated = path.join(cache, 'generated.txt');
  if (fs.existsSync(generated)) {
    for (const f of fs.readFileSync(generated, 'utf8').split(/\r?\n/).filter(Boolean)) fs.rmSync(f, { force: true });
    fs.rmSync(generated, { force: true });
  }
  return res.status ?? 1;
}

if (process.argv[1] && fileURLToPath(import.meta.url) === path.resolve(process.argv[1])) {
  process.exit(main(process.argv.slice(2)));
}
