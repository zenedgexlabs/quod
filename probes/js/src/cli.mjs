#!/usr/bin/env node
// quod-js: runs Vitest or Jest with the Quod probe.
//
//   quod-js record --out capture.jsonl [--runner vitest|jest] [--root DIR] [--dir DIR] [-- <runner args>]
//   quod-js run --skip skip.txt [--runner vitest|jest] [--root DIR] [--dir DIR] [-- <runner args>]
//
// --root is the repository root (paths in the capture are relative to it);
// --dir is where the tests run (a workspace package), default --root.
// --runner defaults to Vitest when a Vitest/Vite config exists or Vitest is
// installed, else Jest.
//
// The user's config is wrapped, not modified: a generated config (under
// node_modules/.cache/quod) loads it, puts the probe's setup file first and
// adds the reporter. The repository's working tree is left untouched, so
// the wrapper can't show up as a change.

import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import { createRequire } from 'node:module';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { digest, environHash, envValueHash, environment, invocation, outerEnv } from './fingerprint.mjs';
import { PROBE_ID } from './version.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const CONFIG_NAMES = ['vitest.config', 'vite.config'];
const CONFIG_EXTS = ['.ts', '.mts', '.cts', '.js', '.mjs', '.cjs'];
const JEST_CONFIG_EXTS = ['.js', '.ts', '.mjs', '.cjs', '.mts', '.cts', '.json'];

function usage(msg) {
  if (msg) console.error(`quod-js: ${msg}`);
  console.error('usage: quod-js record --out FILE [--results FILE] [--runner vitest|jest] [--root DIR] [-- runner args]');
  console.error('       quod-js run --skip FILE [--results FILE] [--runner vitest|jest] [--root DIR] [-- runner args]');
  console.error('       quod-js fingerprint [--root DIR] [--dir DIR] [--env NAME]...');
  process.exit(2);
}

export function parseArgs(argv) {
  const [cmd, ...rest] = argv;
  if (!['record', 'run', 'fingerprint'].includes(cmd)) usage(`unknown command ${cmd ?? ''}`);
  const opts = { cmd, root: process.cwd(), dir: null, out: null, skip: null, results: null, runner: null, env: [], passthrough: [] };
  for (let i = 0; i < rest.length; i++) {
    const a = rest[i];
    if (a === '--') {
      opts.passthrough = rest.slice(i + 1);
      break;
    } else if (a === '--out') opts.out = rest[++i];
    else if (a === '--skip') opts.skip = rest[++i];
    else if (a === '--results') opts.results = rest[++i];
    else if (a === '--runner') opts.runner = rest[++i];
    else if (a === '--root') opts.root = rest[++i];
    else if (a === '--dir') opts.dir = rest[++i];
    else if (a === '--env') opts.env.push(rest[++i]);
    else usage(`unknown option ${a}`);
  }
  if (cmd === 'record' && !opts.out) usage('record needs --out');
  if (cmd === 'run' && !opts.skip) usage('run needs --skip');
  if (opts.runner && !['vitest', 'jest'].includes(opts.runner)) usage(`unknown runner ${opts.runner}`);
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
import { configDefaults, mergeConfig } from 'vitest/config';
import { wrapProjects } from ${JSON.stringify(pathToFileURL(path.join(HERE, 'projects.mjs')).href)};

export default async (env) => {
  const user = typeof userExport === 'function' ? await userExport(env) : await userExport;
  const config = mergeConfig(user ?? {}, { root: ${JSON.stringify(root)} });
  config.test ??= {};
  const userSetup = [].concat(config.test.setupFiles ?? []);
  // The probe's setup file must come first: its hooks bound each test's window.
  config.test.setupFiles = [${JSON.stringify(setup)}, ...userSetup.filter((s) => s !== ${JSON.stringify(setup)})];
  if (process.env.QUOD_RECORD_DIR) {
    // Recording runs one test at a time, so every test gets its own coverage
    // window. Concurrency doesn't change what code a test executes; skip
    // runs keep the project's own concurrency.
    config.test.maxConcurrency = 1;
    config.test.sequence = { ...(config.test.sequence ?? {}), concurrent: false };
    process.env.QUOD_SERIAL = '1';
  }
  if (Array.isArray(config.test.projects)) {
    config.test.projects = wrapProjects(config.test.projects, {
      root: ${JSON.stringify(root)},
      setup: ${JSON.stringify(setup)},
      cacheDir: ${JSON.stringify(cacheDir ?? root)},
    });
  }
  // Test files whose every recorded test is skipped aren't collected at all.
  const skipFiles = JSON.parse(process.env.QUOD_SKIP_FILES || '[]');
  // The globs are relative to this config's root. Inline projects with
  // \`extends: true\` inherit them with the same root; one that sets its own
  // root would match them against another directory, so then nothing is
  // excluded. Other projects (files, non-extending objects) inherit nothing
  // and keep name-level skips.
  const anchored = [].concat(config.test.projects ?? []).every(
    (p) => !(p && typeof p === 'object' && p.extends === true && (p.root || p.test?.root)),
  );
  if (skipFiles.length && anchored) {
    config.test.exclude = [...(config.test.exclude ?? configDefaults.exclude), ...skipFiles];
    // Every file excluded (nothing to run) is a pass, as when every test is skipped by name.
    config.test.passWithNoTests = true;
  }
  const reporters = [].concat(config.test.reporters ?? ['default']);
  config.test.reporters = [...reporters, ${JSON.stringify(reporter)}];
  return config;
};
`;
}

/** The user's Jest config: `--config` in the args, a jest.config.*, or package.json's `jest`. */
function findJestConfig(root, passthrough) {
  const i = passthrough.findIndex((a) => a === '--config' || a === '-c' || a.startsWith('--config='));
  if (i >= 0) {
    const a = passthrough[i];
    const value = a.startsWith('--config=') ? a.slice('--config='.length) : passthrough[i + 1];
    passthrough.splice(i, a.startsWith('--config=') ? 1 : 2);
    return path.resolve(root, value);
  }
  for (const e of JEST_CONFIG_EXTS) {
    const p = path.join(root, `jest.config${e}`);
    if (fs.existsSync(p)) return p;
  }
  try {
    const pkg = path.join(root, 'package.json');
    if (JSON.parse(fs.readFileSync(pkg, 'utf8')).jest) return pkg;
  } catch {
    // no package.json
  }
  return null;
}

function installed(root, name) {
  try {
    createRequire(path.join(root, 'package.json')).resolve(`${name}/package.json`);
    return true;
  } catch {
    return false;
  }
}

/** Vitest if configured or installed, else Jest. */
export function detectRunner(root) {
  if (CONFIG_NAMES.some((n) => CONFIG_EXTS.some((e) => fs.existsSync(path.join(root, n + e))))) return 'vitest';
  if (installed(root, 'vitest')) return 'vitest';
  if (installed(root, 'jest')) return 'jest';
  return 'vitest';
}

/**
 * The generated Jest config: the user's options as Jest itself reads them,
 * with the probe's setup file first and the reporter added.
 */
export function jestWrapperSource({ root, userConfig, setup, reporter, environment }) {
  return `'use strict';
const path = require('node:path');
const { createRequire } = require('node:module');
const req = createRequire(${JSON.stringify(path.join(root, 'package.json'))});
const { readInitialOptions } = req('jest-config');

module.exports = async () => {
  const { config } = await readInitialOptions(${JSON.stringify(userConfig ?? undefined) ?? 'undefined'}, {
    packageRootOrConfig: ${JSON.stringify(root)},
    readFromCwd: false,
    skipMultipleConfigError: true,
  });
  const rootDir = config.rootDir ?? ${JSON.stringify(root)};
  // The Quod environment extends the project's own.
  process.env.QUOD_JEST_BASE_ENV = config.testEnvironment ?? 'node';
  // Test files whose every recorded test is skipped aren't collected at all.
  const skip = JSON.parse(process.env.QUOD_SKIP_PATTERNS || '[]');
  const ignore = skip.length && !config.projects
    ? {
        testPathIgnorePatterns: [...(config.testPathIgnorePatterns ?? ['/node_modules/']), ...skip],
        // Every file excluded (nothing to run) is a pass, as when every test is skipped by name.
        passWithNoTests: true,
      }
    : {};
  return {
    ...config,
    ...ignore,
    rootDir,
    testEnvironment: ${JSON.stringify(environment)},
    setupFilesAfterEnv: [${JSON.stringify(setup)}, ...(config.setupFilesAfterEnv ?? [])],
    reporters: [...(config.reporters ?? ['default']), ${JSON.stringify(reporter)}],
  };
};
`;
}

function jestBin(root) {
  const require = createRequire(path.join(root, 'package.json'));
  const pkgPath = require.resolve('jest/package.json');
  const pkg = JSON.parse(fs.readFileSync(pkgPath, 'utf8'));
  const bin = typeof pkg.bin === 'string' ? pkg.bin : pkg.bin.jest;
  return path.join(path.dirname(pkgPath), bin);
}

function vitestBin(root) {
  const require = createRequire(path.join(root, 'package.json'));
  const pkgPath = require.resolve('vitest/package.json');
  const pkg = JSON.parse(fs.readFileSync(pkgPath, 'utf8'));
  const bin = typeof pkg.bin === 'string' ? pkg.bin : pkg.bin.vitest;
  return path.join(path.dirname(pkgPath), bin);
}

/**
 * Re-checks the conditions the certificates assumed (skip-file header
 * lines), in the environment the tests are about to run in. Returns the
 * first mismatch, or null.
 */
export function checkSkipFile(file, { root, dir, args, runner }) {
  let envFp = null;
  for (const line of fs.readFileSync(file, 'utf8').split(/\r?\n/)) {
    if (!line.startsWith('# quod-')) continue;
    const [kind, ...rest] = line.slice('# quod-'.length).trim().split(/\s+/);
    if (kind === 'environment' && rest.length === 1) {
      envFp ??= environment(root, dir);
      if (digest(envFp) !== rest[0]) return 'the environment differs from the one the plan was made for';
    } else if (kind === 'invocation' && rest.length === 1) {
      if (digest(invocation(args, { runner, root, dir })) !== rest[0]) return 'the test runner invocation differs from the recorded one';
    } else if (kind === 'skip-file') {
      // Not a condition: a file the runner needn't collect (skipFiles).
    } else if (kind === 'env' && rest.length === 2) {
      const [name, expected] = rest;
      const actual = name === '*' ? environHash() : envValueHash(process.env[name]);
      if (actual !== expected) return `environment variable ${name} differs`;
    } else {
      return `unknown skip-file check: ${line}`;
    }
  }
  return null;
}

/**
 * `# quod-skip-file PATH` lines (repository-relative) as exclude globs
 * relative to the runner's directory. Files outside it are left alone.
 */
export function skipFiles(file, { root, dir }) {
  const out = [];
  for (const line of fs.readFileSync(file, 'utf8').split(/\r?\n/)) {
    if (!line.startsWith('# quod-skip-file ')) continue;
    const r = path.relative(dir, path.join(root, line.slice('# quod-skip-file '.length).trim()));
    if (!r || r.startsWith('..') || path.isAbsolute(r)) continue;
    // Escape glob syntax: the path is matched literally.
    out.push(r.split(path.sep).join('/').replace(/[\\*?[\]{}()!+@]/g, '\\$&'));
  }
  return out;
}

/**
 * `# quod-skip-file PATH` lines as Jest `testPathIgnorePatterns`: regexes
 * matching exactly that absolute path, with either path separator.
 */
export function jestIgnorePatterns(file, { root }) {
  const meta = (s) => s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  // Windows paths are case-insensitive (the drive letter's case varies).
  const esc =
    process.platform === 'win32'
      ? (s) => meta(s).replace(/[a-zA-Z]/g, (c) => `[${c.toLowerCase()}${c.toUpperCase()}]`)
      : meta;
  const out = [];
  for (const line of fs.readFileSync(file, 'utf8').split(/\r?\n/)) {
    if (!line.startsWith('# quod-skip-file ')) continue;
    const abs = path.resolve(root, line.slice('# quod-skip-file '.length).trim());
    out.push(`^${abs.split(/[\\/]/).map(esc).join('[\\\\/]')}$`);
  }
  return out;
}

export function main(argv) {
  const opts = parseArgs(argv);
  if (opts.cmd === 'fingerprint') {
    const env = Object.fromEntries(
      opts.env.map((n) => [n, n === '*' ? environHash() : envValueHash(process.env[n])]),
    );
    const fp = environment(opts.root, opts.dir);
    // environment_digest: what the runner re-checks before skipping anything.
    process.stdout.write(`${JSON.stringify({ probe: PROBE_ID, environment: fp, environment_digest: digest(fp), env })}\n`);
    return 0;
  }
  const runner = opts.runner ?? detectRunner(opts.dir);
  const jest = runner === 'jest';
  const userConfig = jest ? findJestConfig(opts.dir, opts.passthrough) : findUserConfig(opts.dir, opts.passthrough);
  const cache = path.join(opts.dir, 'node_modules', '.cache', 'quod');
  fs.mkdirSync(cache, { recursive: true });
  const wrapper = path.join(cache, jest ? 'jest.config.cjs' : 'vitest.config.mjs');
  fs.writeFileSync(
    wrapper,
    jest
      ? jestWrapperSource({
          root: opts.dir,
          userConfig,
          setup: path.join(HERE, 'jest-setup.cjs'),
          reporter: path.join(HERE, 'jest-reporter.mjs'),
          environment: path.join(HERE, 'jest-env.cjs'),
        })
      : wrapperSource({
          root: opts.dir,
          userConfig,
          setup: path.join(HERE, 'setup.mjs'),
          reporter: path.join(HERE, 'reporter.mjs'),
          cacheDir: cache,
        }),
  );
  const env = {
    ...process.env,
    QUOD_ROOT: opts.root,
    QUOD_DIR: opts.dir,
    QUOD_ARGS: JSON.stringify(opts.passthrough),
  };
  if (opts.results) env.QUOD_RESULTS_OUT = path.resolve(opts.results);
  let recordDir = null;
  if (opts.cmd === 'record') {
    recordDir = fs.mkdtempSync(path.join(os.tmpdir(), `quod-${runner}-`));
    // Jest: one test at a time within a file, so windows never overlap.
    if (jest) env.QUOD_SERIAL = '1';
    env.QUOD_RECORD_DIR = recordDir;
    env.QUOD_RECORD_OUT = path.resolve(opts.out);
    env.QUOD_ROOT_FILES = JSON.stringify(
      userConfig ? [path.relative(opts.root, userConfig).split(path.sep).join('/')] : [],
    );
    // Fingerprints in this process, before the runner changes its environment.
    try {
      const fp = path.join(recordDir, 'fingerprint.json');
      fs.writeFileSync(
        fp,
        JSON.stringify({
          environment: environment(opts.root, opts.dir),
          invocation: invocation(opts.passthrough, { runner, root: opts.root, dir: opts.dir }),
          env: outerEnv(),
        }),
      );
      env.QUOD_FINGERPRINT_FILE = fp;
    } catch (e) {
      process.stderr.write(`quod: no environment fingerprint (${e?.message ?? e})\n`);
    }
  } else {
    let problem;
    try {
      problem = checkSkipFile(opts.skip, { root: opts.root, dir: opts.dir, args: opts.passthrough, runner });
    } catch (e) {
      problem = `could not check the skip file (${e?.message ?? e})`;
    }
    if (problem) process.stderr.write(`quod: not skipping any tests: ${problem}\n`);
    else {
      env.QUOD_SKIP_FILE = path.resolve(opts.skip);
      if (jest) env.QUOD_SKIP_PATTERNS = JSON.stringify(jestIgnorePatterns(opts.skip, { root: opts.root }));
      else env.QUOD_SKIP_FILES = JSON.stringify(skipFiles(opts.skip, { root: opts.root, dir: opts.dir }));
    }
  }
  const runArgs = jest
    ? [jestBin(opts.dir), '--config', wrapper, ...(opts.cmd === 'record' ? ['--maxConcurrency=1'] : []), ...opts.passthrough]
    : [vitestBin(opts.dir), 'run', '--config', wrapper, ...opts.passthrough];
  const res = spawnSync(process.execPath, runArgs, {
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
