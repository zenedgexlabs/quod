#!/usr/bin/env node
// skipcert-vitest: runs Vitest with the Skipcert probe.
//
//   skipcert-vitest record --out capture.jsonl [--root DIR] [-- <vitest args>]
//   skipcert-vitest run --skip skip.txt [--root DIR] [-- <vitest args>]
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
  const opts = { cmd, root: process.cwd(), out: null, skip: null, passthrough: [] };
  for (let i = 0; i < rest.length; i++) {
    const a = rest[i];
    if (a === '--') {
      opts.passthrough = rest.slice(i + 1);
      break;
    } else if (a === '--out') opts.out = rest[++i];
    else if (a === '--skip') opts.skip = rest[++i];
    else if (a === '--root') opts.root = rest[++i];
    else usage(`unknown option ${a}`);
  }
  if (cmd === 'record' && !opts.out) usage('record needs --out');
  if (cmd === 'run' && !opts.skip) usage('run needs --skip');
  opts.root = path.resolve(opts.root);
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

export function wrapperSource({ root, userConfig, setup, reporter }) {
  const imp = userConfig
    ? `import userExport from ${JSON.stringify(pathToFileURL(userConfig).href)};`
    : 'const userExport = {};';
  return `${imp}
import { mergeConfig } from 'vitest/config';

export default async (env) => {
  const user = typeof userExport === 'function' ? await userExport(env) : await userExport;
  const config = mergeConfig(user ?? {}, { root: ${JSON.stringify(root)} });
  config.test ??= {};
  const userSetup = [].concat(config.test.setupFiles ?? []);
  // The probe's setup file must come first: its hooks bound each test's window.
  config.test.setupFiles = [${JSON.stringify(setup)}, ...userSetup.filter((s) => s !== ${JSON.stringify(setup)})];
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
  const userConfig = findUserConfig(opts.root, opts.passthrough);
  const cache = path.join(opts.root, 'node_modules', '.cache', 'skipcert');
  fs.mkdirSync(cache, { recursive: true });
  const wrapper = path.join(cache, 'vitest.config.mjs');
  fs.writeFileSync(
    wrapper,
    wrapperSource({
      root: opts.root,
      userConfig,
      setup: path.join(HERE, 'setup.mjs'),
      reporter: path.join(HERE, 'reporter.mjs'),
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
  const res = spawnSync(process.execPath, [vitestBin(opts.root), 'run', '--config', wrapper, ...opts.passthrough], {
    cwd: opts.root,
    env,
    stdio: 'inherit',
  });
  if (recordDir) fs.rmSync(recordDir, { recursive: true, force: true });
  return res.status ?? 1;
}

if (process.argv[1] && fileURLToPath(import.meta.url) === path.resolve(process.argv[1])) {
  process.exit(main(process.argv.slice(2)));
}
