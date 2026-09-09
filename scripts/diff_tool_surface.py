#!/usr/bin/env python3
"""Diff the Rust server's advertised tool surface against the Python implementation's.

The two servers are meant to be interchangeable, and the surface an agent sees — tool names,
descriptions, argument names, argument order, which arguments are required, and every default — is
the contract that has to match. No test inside either language can check that: the Python side is
the source of truth and the Rust side only exists at runtime, over stdio.

So this reads the Python signatures and docstrings straight out of the AST (no import, no
credentials), starts the Rust binary, asks it for `tools/list`, and compares. Exits non-zero on any
mismatch, so it can gate a release.

    python3 scripts/diff_tool_surface.py                     # uses rust/target/release/gdrive-mcp
    python3 scripts/diff_tool_surface.py path/to/gdrive-mcp

`cargo build --release --manifest-path rust/Cargo.toml` first if the binary is stale.
"""

from __future__ import annotations

import ast
import inspect
import json
import pathlib
import subprocess
import sys

REPO = pathlib.Path(__file__).resolve().parent.parent
TOOL_MODULES = ("discovery", "sheets", "docs", "files", "calendar")
DEFAULT_BINARY = REPO / "rust" / "target" / "release" / "gdrive-mcp"

# Module-level constants used as parameter defaults. Resolved by name because the AST holds the
# reference, not the value; keep in step with src/gdrive_mcp/chunking.py.
NAMED_DEFAULTS = {"DEFAULT_MAX_CHARS": 8000}


def _default(node: ast.expr):
    if isinstance(node, ast.Name) and node.id in NAMED_DEFAULTS:
        return NAMED_DEFAULTS[node.id]
    return ast.literal_eval(node)


def python_surface() -> dict[str, dict]:
    """Every registered Python tool, read out of the AST in `_TOOLS` order."""
    out: dict[str, dict] = {}
    for module in TOOL_MODULES:
        tree = ast.parse((REPO / "src" / "gdrive_mcp" / "tools" / f"{module}.py").read_text())
        registered = next(
            ([el.id for el in node.value.elts]
             for node in tree.body
             if isinstance(node, ast.Assign) and getattr(node.targets[0], "id", "") == "_TOOLS"),
            [],
        )
        functions = {n.name: n for n in tree.body if isinstance(n, ast.FunctionDef)}
        for name in registered:
            fn = functions[name]
            params = [a.arg for a in fn.args.args]
            defaults = fn.args.defaults
            out[name] = {
                # FastMCP used inspect.getdoc, i.e. cleandoc, for the agent-facing description.
                "description": inspect.cleandoc(ast.get_docstring(fn, clean=False) or ""),
                "params": params,
                "required": params[: len(params) - len(defaults)],
                "defaults": {a: _default(d) for a, d in zip(params[len(params) - len(defaults):], defaults)},
            }
    return out


def rust_surface(binary: pathlib.Path) -> dict[str, dict]:
    """Every tool the Rust server advertises, over a real stdio MCP handshake."""
    if not binary.exists():
        sys.exit(f"binary not found: {binary}\nbuild it: cargo build --release --manifest-path rust/Cargo.toml")
    request = "\n".join([
        json.dumps({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "surface-diff", "version": "0"}}}),
        json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json.dumps({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    ]) + "\n"
    # A token is never needed: tools/list does not touch Google. Point the cache at nothing so a
    # real one is not read, and sandbox the files dir so startup cannot sweep anyone's spills.
    env = {
        **dict(__import__("os").environ),
        "GDRIVE_MCP_TOKEN": "/nonexistent/surface-diff-token.json",
        "GDRIVE_MCP_FILES_DIR": "/tmp/gdrive-mcp-surface-diff/files",
    }
    proc = subprocess.run([str(binary), "serve"], input=request, capture_output=True, text=True, env=env)
    for line in proc.stdout.splitlines():
        message = json.loads(line)
        if message.get("id") == 2:
            return {t["name"]: t for t in message["result"]["tools"]}
    sys.exit(f"no tools/list response from {binary}\nstderr:\n{proc.stderr}")


def main(argv: list[str]) -> int:
    binary = pathlib.Path(argv[1]) if len(argv) > 1 else DEFAULT_BINARY
    py, rs = python_surface(), rust_surface(binary)
    problems: list[str] = []

    for name in sorted(set(py) - set(rs)):
        problems.append(f"{name}: registered in Python, missing from Rust")
    for name in sorted(set(rs) - set(py)):
        problems.append(f"{name}: advertised by Rust, absent from Python")

    for name in sorted(set(py) & set(rs)):
        want, got = py[name], rs[name]
        schema = got["inputSchema"]
        properties = list(schema.get("properties", {}))

        if got["description"].strip() != want["description"].strip():
            problems.append(f"{name}: description differs from the Python docstring")
        # Order matters as much as membership: it is the order an agent reads them in.
        if properties != want["params"]:
            problems.append(f"{name}: arguments {properties} != Python {want['params']}")
        if sorted(schema.get("required", [])) != sorted(want["required"]):
            problems.append(
                f"{name}: required {sorted(schema.get('required', []))} != Python {sorted(want['required'])}"
            )
        # `X | None = None` is optional, which JSON Schema says by omission from `required`; a
        # null default would be a schema that contradicts its own declared type.
        for arg, py_default in want["defaults"].items():
            if arg not in schema.get("properties", {}):
                continue  # already reported as a membership problem
            rs_default = schema["properties"][arg].get("default", "<absent>")
            expected = "<absent>" if py_default is None else py_default
            if rs_default != expected:
                problems.append(f"{name}.{arg}: default {rs_default!r} != Python {py_default!r}")
        if schema.get("additionalProperties") is not False:
            problems.append(f"{name}: schema does not forbid unknown arguments")

    if problems:
        print(f"{len(problems)} surface mismatch(es):", file=sys.stderr)
        for problem in problems:
            print(f"  - {problem}", file=sys.stderr)
        return 1
    print(f"tool surfaces match: {len(py)} tools, identical names, descriptions, arguments, "
          f"required sets and defaults")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
