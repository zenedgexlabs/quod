// V8 precise coverage -> executed original source lines.
//
// Each take of `Profiler.takePreciseCoverage` (callCount: true, detailed:
// true) returns, per script, nested ranges with execution counts since the
// previous take. The innermost range containing a byte decides whether it
// ran. Executed bytes of the *generated* code (what Vite or a transpiler
// produced) are mapped back to original lines through the module's source
// map. Anything that can't be mapped precisely is over-approximated: the
// whole original file counts as executed, which can only make more tests run.

const B64 = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
const B64_INDEX = new Map([...B64].map((c, i) => [c, i]));

/** Decodes source map `mappings`: per generated line, [genCol, srcIdx, origLine, origCol] segments (0-based). */
export function decodeMappings(mappings) {
  const lines = [];
  let src = 0;
  let oLine = 0;
  let oCol = 0;
  for (const lineText of mappings.split(';')) {
    const segs = [];
    let gCol = 0;
    if (lineText) {
      for (const seg of lineText.split(',')) {
        if (!seg) continue;
        const v = [];
        let value = 0;
        let shift = 0;
        for (const ch of seg) {
          const d = B64_INDEX.get(ch);
          if (d === undefined) break;
          value += (d & 31) << shift;
          if (d & 32) {
            shift += 5;
          } else {
            v.push(value & 1 ? -(value >>> 1) : value >>> 1);
            value = 0;
            shift = 0;
          }
        }
        gCol += v[0] ?? 0;
        if (v.length >= 4) {
          src += v[1];
          oLine += v[2];
          oCol += v[3];
          segs.push([gCol, src, oLine, oCol]);
        } else {
          segs.push([gCol, -1, -1, -1]);
        }
      }
    }
    lines.push(segs);
  }
  return lines;
}

/** Byte-level "ran" mask of a script from its V8 function ranges. */
export function executedMask(functions, length) {
  const ranges = [];
  for (const f of functions) for (const r of f.ranges) ranges.push(r);
  // Outer ranges first, so nested ranges overwrite them.
  ranges.sort((a, b) => a.startOffset - b.startOffset || b.endOffset - a.endOffset);
  const mask = new Uint8Array(length);
  for (const r of ranges) {
    const end = Math.min(r.endOffset, length);
    if (r.startOffset < end) mask.fill(r.count > 0 ? 1 : 0, r.startOffset, end);
  }
  return mask;
}

function lineStarts(text) {
  const starts = [0];
  for (let i = 0; i < text.length; i++) if (text.charCodeAt(i) === 10) starts.push(i + 1);
  return starts;
}

function anyRan(mask, from, to) {
  for (let i = Math.max(0, from); i < to && i < mask.length; i++) if (mask[i]) return true;
  return false;
}

/**
 * Executed original lines (1-based) of one script.
 *
 * - `scriptSource`: what V8 ran (may wrap `code`).
 * - `code`: the generated code the source map describes (`null` if unknown).
 * - `map`: `{ mappings, sources }` or `null` (generated == original).
 * - `originalLineCount`: number of lines in the original file.
 *
 * Returns `{ lines: Set<number> }`, or `{ all: true }` when the mapping is
 * unknown (over-approximation).
 */
export function executedLines({ functions, scriptSource, code, map, originalLineCount }) {
  const mask = executedMask(functions, scriptSource.length);
  const text = code ?? scriptSource;
  const delta = code == null ? 0 : scriptSource.indexOf(code);
  if (delta < 0) return { all: true };
  const starts = lineStarts(text);
  const lines = new Set();
  if (!map) {
    // Generated code is the original file.
    for (let l = 0; l < starts.length; l++) {
      const from = delta + starts[l];
      const to = delta + (l + 1 < starts.length ? starts[l + 1] : text.length);
      if (anyRan(mask, from, to)) lines.add(l + 1);
    }
    return { lines };
  }
  const decoded = decodeMappings(map.mappings || '');
  for (let g = 0; g < decoded.length && g < starts.length; g++) {
    const segs = decoded[g];
    const lineStart = delta + starts[g];
    const lineEnd = delta + (g + 1 < starts.length ? starts[g + 1] : text.length);
    for (let i = 0; i < segs.length; i++) {
      const [col, srcIdx, oLine] = segs[i];
      if (srcIdx !== 0 || oLine < 0) continue; // a single-source map (one module)
      const from = lineStart + col;
      const to = i + 1 < segs.length ? lineStart + segs[i + 1][0] : lineEnd;
      if (anyRan(mask, from, Math.max(to, from + 1))) {
        if (oLine + 1 <= originalLineCount) lines.add(oLine + 1);
      }
    }
  }
  return { lines };
}

/**
 * Functions that ran in one script since the last take, as original start
 * positions (1-based line, 0-based column), plus whether module-level code ran.
 *
 * V8 reports every function with its source range; a function ran if its
 * outermost range has a positive count. The script itself and any wrapper the
 * module runner put around the module (everything starting before `code`)
 * count as module-level code. Returns `{ all: true }` when a position can't be
 * mapped, which callers over-approximate.
 */
export function executedFunctions({ functions, scriptSource, code, map }) {
  const delta = code == null ? 0 : scriptSource.indexOf(code);
  if (delta < 0) return { all: true };
  const text = code ?? scriptSource;
  const starts = lineStarts(text);
  const decoded = map ? decodeMappings(map.mappings || '') : null;
  const out = { starts: [], module: false };
  for (const f of functions) {
    const top = f.ranges[0];
    if (!top || top.count <= 0) continue;
    const off = top.startOffset - delta;
    // The script's own entry covers the whole script. A function can also
    // start at offset 0 (Jest 30 and Node compile modules without a
    // wrapper), so the start alone doesn't identify the script.
    const isScript = top.startOffset === 0 && top.endOffset >= scriptSource.length;
    if (off < 0 || isScript) {
      out.module = true;
      continue;
    }
    if (off >= text.length) return { all: true };
    // Generated (line, col) of the function start.
    let lo = 0;
    let hi = starts.length - 1;
    while (lo < hi) {
      const mid = (lo + hi + 1) >> 1;
      if (starts[mid] <= off) lo = mid;
      else hi = mid - 1;
    }
    const gLine = lo;
    const gCol = off - starts[lo];
    if (!decoded) {
      out.starts.push([gLine + 1, gCol]);
      continue;
    }
    const segs = decoded[gLine] || [];
    let best = null;
    for (const s of segs) {
      if (s[1] !== 0 || s[2] < 0) continue;
      if (s[0] <= gCol) best = s;
      else break;
    }
    if (!best) {
      // No mapping at or before the start on its line: take the first mapped
      // segment of the line (the function begins there in the original).
      best = segs.find((s) => s[1] === 0 && s[2] >= 0);
      if (!best) {
        // An unmapped line is code the transform injected (export getters,
        // helpers), not part of the original source: it runs on behalf of
        // the module as a whole.
        out.module = true;
        continue;
      }
      out.starts.push([best[2] + 1, best[3]]);
      continue;
    }
    out.starts.push([best[2] + 1, best[3] + (gCol - best[0])]);
  }
  return out;
}

/** Inclusive [start, end] ranges from a set of line numbers. */
export function toRanges(lines) {
  const sorted = [...lines].sort((a, b) => a - b);
  const out = [];
  for (const l of sorted) {
    const last = out[out.length - 1];
    if (last && l === last[1] + 1) last[1] = l;
    else out.push([l, l]);
  }
  return out;
}
