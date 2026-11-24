// Quod Vitest worker setup (ADR 013). Must be the *first* setup file.
//
// Record mode (QUOD_RECORD_DIR set): per-test coverage windows, via the
// shared recorder (recorder.mjs), which documents the attribution rules.
// Skip mode (QUOD_SKIP_FILE set): tests named in the file are skipped
// before their own hooks run.

import { afterAll, afterEach, beforeEach, vi } from 'vitest';
import path from 'node:path';
import { createRecorder, readSkipFile } from './recorder.mjs';

const ROOT = path.resolve(process.env.QUOD_ROOT || process.cwd());
const RECORD_DIR = process.env.QUOD_RECORD_DIR;
const SKIP_FILE = process.env.QUOD_SKIP_FILE;

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
  const skip = readSkipFile(SKIP_FILE);
  beforeEach((ctx) => {
    if (skip.has(testName(ctx.task))) ctx.skip();
  });
}

// --- record mode -----------------------------------------------------------

/** Generated code and source map from Vite's module runner. */
function moduleFor(abs) {
  const em = globalThis.__vitest_worker__?.evaluatedModules;
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

if (RECORD_DIR) {
  const rec = await createRecorder({ root: ROOT, recordDir: RECORD_DIR, moduleFor }).module();

  // vi.stubEnv reads the old value only to restore it later.
  try {
    const orig = vi.stubEnv;
    vi.stubEnv = function (name, ...rest) {
      rec.markWritten(name);
      return orig.call(this, name, ...rest);
    };
  } catch {
    // a frozen `vi`: stubbed reads stay recorded (conservative)
  }

  beforeEach(async (ctx) =>
    rec.before(testName(ctx.task), {
      moduleFile: ctx.task.file.filepath,
      // Concurrent tests share windows, unless recording serialised them.
      concurrent: ctx.task.concurrent && process.env.QUOD_SERIAL !== '1',
    }),
  );
  afterEach(async (ctx) => rec.after(testName(ctx.task)));
  afterAll(async () => rec.finish());
}
