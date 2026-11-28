"""Mutants of one function: small, valid edits a test suite should notice.

``quod mutate`` (ADR 025) applies them one at a time to a function a change
touched and runs only the tests that execute it. A mutant no test notices
(it *survives*) is logic the change added or altered that nothing checks.

Mutation sites are tokens in the function's own body: comparison and
arithmetic operators, ``and``/``or``, ``True``/``False`` and small integers.
Docstrings and nested ``def``/``class`` bodies are left out (they are
entities of their own). Every mutant is checked to compile, so none is a
syntax error.
"""

from __future__ import annotations

import ast
import io
import tokenize

_SWAPS = {
    "==": "!=",
    "!=": "==",
    "<": ">=",
    ">": "<=",
    "<=": ">",
    ">=": "<",
    "+": "-",
    "-": "+",
    "*": "//",
    "//": "*",
    "+=": "-=",
    "-=": "+=",
    "True": "False",
    "False": "True",
    "and": "or",
    "or": "and",
}


def find(tree: ast.AST, qual: str):
    """The ``def`` whose qualified name (as the probe records it, with
    ``<locals>`` for nested functions) is ``qual``, or None."""

    def walk(node, prefix):
        for child in ast.iter_child_nodes(node):
            if isinstance(child, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
                name = f"{prefix}{child.name}"
                if name == qual and not isinstance(child, ast.ClassDef):
                    return child
                inner = f"{name}.<locals>." if not isinstance(child, ast.ClassDef) else f"{name}."
                found = walk(child, inner)
                if found is not None:
                    return found
            else:
                found = walk(child, prefix)
                if found is not None:
                    return found
        return None

    return walk(tree, "")


def _own_lines(fn) -> set[int]:
    body = fn.body
    if not body:
        return set()
    start = body[0].lineno
    first = body[0]
    if isinstance(first, ast.Expr) and isinstance(first.value, ast.Constant) and isinstance(first.value.value, str):
        start = (first.end_lineno or first.lineno) + 1
    lines = set(range(start, (fn.end_lineno or start) + 1))
    for node in ast.walk(fn):
        if node is not fn and isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
            first_line = min([d.lineno for d in node.decorator_list] + [node.lineno])
            lines -= set(range(first_line, (node.end_lineno or node.lineno) + 1))
    return lines


def sites(source: str, qual: str) -> list[dict]:
    """Mutants of ``qual`` in ``source``: line (1-based), col and end (0-based
    characters on that line), the token and its replacement."""
    tree = ast.parse(source)
    fn = find(tree, qual)
    if fn is None:
        return []
    lines = _own_lines(fn)
    src_lines = source.splitlines(keepends=True)
    out = []
    for tok in tokenize.generate_tokens(io.StringIO(source).readline):
        (line, col), (end_line, end) = tok.start, tok.end
        if line not in lines or line != end_line:
            continue
        new = None
        if tok.type == tokenize.OP or (tok.type == tokenize.NAME and tok.string in ("True", "False", "and", "or")):
            new = _SWAPS.get(tok.string)
        elif tok.type == tokenize.NUMBER and tok.string.isdigit() and len(tok.string) < 4:
            new = str(int(tok.string) + 1)
        if new is None:
            continue
        mutated = src_lines[:]
        text = mutated[line - 1]
        mutated[line - 1] = text[:col] + new + text[end:]
        try:
            compile("".join(mutated), "<mutant>", "exec")
        except SyntaxError:
            continue  # for example `*` in `*args`
        out.append({"line": line, "col": col, "end": end, "old": tok.string, "new": new})
    return out
