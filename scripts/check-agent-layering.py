#!/usr/bin/env python3
"""Layering check for the `querymt-agent` crate split.

Implements task 0.2 of the `split-agent-crate` OpenSpec change (design D1-D3).

Every source file under `crates/agent/src` is mapped to the layer it will live
in once the split is done (design D1). The script then finds every `crate::`,
`super::` and `self::` path in *production* code (everything outside
`#[cfg(test)]`), resolves it through `mod.rs` re-exports to the file that
defines the item, and reports the references that point to a layer the source
layer may not depend on (an "upward" reference).

Layers and the production edges Cargo will allow (design D1):

    core        -
    index       -> core
    knowledge   -> core, index
    sqlite      -> core
    services    -> core, index, knowledge        (dev: sqlite)
    runtime     -> services, knowledge, index, core   (dev: sqlite)
    top         -> runtime, sqlite (and everything below runtime)

The count of *distinct upward items* per layer pair is compared with the
committed allowance in `scripts/agent-layering-allowance.json`. The check fails
when a pair is above its allowance. Lowering the allowance is how each cut is
recorded; raising it needs `--write-allowance --allow-increase`.

Files that contain no definitions at all (`mod.rs` and `lib.rs` re-export
hubs, `prelude.rs`) are facades: they are never a source of violations, and
references through them are followed to the defining file.

Usage:
    scripts/check-agent-layering.py                 check against the allowance
    scripts/check-agent-layering.py --verbose       list every upward item and its sites
    scripts/check-agent-layering.py --tests         also report test-only upward edges (info)
    scripts/check-agent-layering.py --layers        count files per layer
    scripts/check-agent-layering.py --write-allowance   record the current counts

Exit status: 0 ok, 1 above the allowance, 2 usage error.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from collections import defaultdict
from pathlib import Path
from typing import Dict, Iterable, List, NamedTuple, Optional, Set, Tuple

REPO = Path(__file__).resolve().parent.parent
DEFAULT_SRC = REPO / "crates" / "agent" / "src"
DEFAULT_ALLOWANCE = REPO / "scripts" / "agent-layering-allowance.json"

LAYERS = ["core", "index", "knowledge", "sqlite", "services", "runtime", "top"]

# Direct production dependencies (design D1). Closed transitively below.
DIRECT: Dict[str, Set[str]] = {
    "core": set(),
    "index": {"core"},
    "knowledge": {"core", "index"},
    "sqlite": {"core"},
    "services": {"core", "index", "knowledge"},
    "runtime": {"services", "knowledge", "index", "core"},
    "top": {"runtime", "sqlite"},
}
# Extra edges that exist for test code only (`-. dev .->` in the D1 diagram).
DEV_EXTRA: Dict[str, Set[str]] = {
    "services": {"sqlite"},
    "runtime": {"sqlite"},
}

# File -> layer, first match wins (design D1 table). Paths are relative to
# `crates/agent/src` with forward slashes. Everything else is `runtime`.
RULES: List[Tuple[str, str]] = [
    # Pre-extraction layouts (before tasks 1.2 and 1.9 moved these out). They
    # let the script be run on an old tree to reproduce the D3 inventory.
    (r"^index/", "index"),
    (r"^knowledge/repository/", "knowledge"),
    (r"^config/repository_knowledge\.rs$", "knowledge"),
    (r"^dotagents/(frontmatter|layer|diagnostics)\.rs$", "core"),
    (r"^core_staging/", "core"),
    (r"^sqlite_staging/", "sqlite"),
    (r"^(hash|events|model)\.rs$", "core"),
    (
        r"^session/(domain|domain_schedule|error|store|projection|repository|"
        r"backend|lifecycle|location|ranges)\.rs$",
        "core",
    ),
    (r"^session/backend/contract\.rs$", "core"),
    (r"^worktree/domain\.rs$", "core"),
    (r"^delegation/model_overrides\.rs$", "core"),
    (r"^knowledge/(mod|text_processing)\.rs$", "core"),
    (r"^session/sqlite_storage/", "sqlite"),
    (r"^session/repo_[a-z_]+\.rs$", "sqlite"),
    (r"^session/schema\.rs$", "sqlite"),
    (r"^knowledge/sqlite\.rs$", "sqlite"),
    (r"^worktree/repo_workspace\.rs$", "sqlite"),
    (
        r"^session/(task_service|history_window|turns|pruning|runtime|"
        r"run_lease|remote_sync)\.rs$",
        "services",
    ),
    (r"^control/delegation_notifications\.rs$", "services"),
    (r"^(accounting|worktree|snapshot|export|tools|verification)(/|\.rs$)", "services"),
    (
        r"^(workspace_query|model_heuristics|model_info|event_fanout|event_sink|"
        r"session_event_stream|tasks)\.rs$",
        "services",
    ),
    (
        r"^(api/(sessions|session|callbacks)\.rs|api/mesh/(invite|types|runtime|runtime_config)\.rs|"
        r"plugin_update\.rs)$",
        "runtime",
    ),
    (r"^(acp/websocket|runtime_tests/)", "top"),
    (r"^(api|server|runner|quorum|profiles)(/|\.rs$)", "top"),
]
DEFAULT_LAYER = "runtime"

# Paths that are crate re-exports rather than modules: prefix -> layer.
ALIASES: Dict[Tuple[str, ...], str] = {
    ("hash",): "core",
    ("index",): "index",
    ("knowledge", "repository"): "knowledge",
    # Since task 4.1 the core layer is the crate `querymt-agent-core`; these
    # paths are facade aliases of it inside `querymt-agent`.
    ("core_staging",): "core",
    ("querymt_agent_sqlite",): "sqlite",
    ("session", "sqlite_storage"): "sqlite",
    ("session", "schema"): "sqlite",
    ("session", "repo_artifact"): "sqlite",
    ("session", "repo_decision"): "sqlite",
    ("session", "repo_delegation"): "sqlite",
    ("session", "repo_intent"): "sqlite",
    ("session", "repo_progress"): "sqlite",
    ("session", "repo_schedule"): "sqlite",
    ("session", "repo_session"): "sqlite",
    ("session", "repo_task"): "sqlite",
    ("knowledge", "sqlite"): "sqlite",
    ("worktree", "repo_workspace"): "sqlite",
    ("querymt_agent_core",): "core",
    ("events",): "core",
    ("model",): "core",
    ("session", "domain"): "core",
    ("session", "domain_schedule"): "core",
    ("session", "error"): "core",
    ("session", "lifecycle"): "core",
    ("session", "location"): "core",
    ("session", "projection"): "core",
    ("session", "ranges"): "core",
    ("session", "repository"): "core",
    ("session", "store"): "core",
    ("session", "backend"): "core",
}

# Files in these directories must not mention SQLite at all (task 3.6).
SQLITE_FREE_DIRS = ("core_staging/",)
SQLITE_MENTION = re.compile(r"\brusqlite\b|Sqlite")

# Core must not depend on these crates (design D7, task 4.0d). Checked in every
# file mapped to the core layer, outside tests and comments.
CORE_FORBIDDEN = re.compile(r"\b(rmcp|querymt_utils|rusqlite)\b")


def closure(layer: str) -> Set[str]:
    seen: Set[str] = set()
    todo = [layer]
    while todo:
        current = todo.pop()
        for dep in DIRECT[current]:
            if dep not in seen:
                seen.add(dep)
                todo.append(dep)
    return seen


ALLOWED = {layer: closure(layer) for layer in LAYERS}
ALLOWED_WITH_DEV = {
    layer: set().union(
        ALLOWED[layer],
        *[closure(d) | {d} for d in DEV_EXTRA.get(layer, set())],
    )
    for layer in LAYERS
}


def layer_of_path(rel: str) -> str:
    for pattern, layer in RULES:
        if re.search(pattern, rel):
            return layer
    return DEFAULT_LAYER


# --------------------------------------------------------------------------
# Lexing helpers


_TOKEN = re.compile(
    r"(?P<line>//[^\n]*)"
    r"|(?P<block>/\*.*?\*/)"
    r"|(?<![A-Za-z0-9_])(?P<raw>b?r(?P<h>#*)\".*?\"(?P=h))"
    r"|(?P<str>b?\"(?:\\.|[^\"\\])*\")"
    r"|(?P<chr>b?'(?:\\(?:x[0-9a-fA-F]{2}|u\{[0-9a-fA-F_]+\}|.)|[^\\'\n])')",
    re.S,
)


def _blank(match: "re.Match[str]") -> str:
    return re.sub(r"[^\n]", " ", match.group(0))


def strip_code(text: str) -> str:
    """Blank comments and string/char literals, keeping offsets and newlines."""
    return _TOKEN.sub(_blank, text)


_CFG_TEST = re.compile(
    r"#\s*\[\s*cfg\s*\(\s*(?:test\b|any\s*\(\s*test\b|all\s*\(\s*test\b)"
)
_INNER_CFG_TEST = re.compile(r"#\s*!\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]")


def _match_bracket(s: str, i: int) -> int:
    """`s[i]` is `[`; return the index just past the matching `]`."""
    depth = 0
    n = len(s)
    while i < n:
        if s[i] == "[":
            depth += 1
        elif s[i] == "]":
            depth -= 1
            if depth == 0:
                return i + 1
        i += 1
    return n


def _match_brace(s: str, i: int) -> int:
    """`s[i]` is `{`; return the index just past the matching `}`."""
    depth = 0
    n = len(s)
    while i < n:
        if s[i] == "{":
            depth += 1
        elif s[i] == "}":
            depth -= 1
            if depth == 0:
                return i + 1
        i += 1
    return n


def _item_end(s: str, i: int) -> int:
    """End of the item starting at `i`: a `;` or a balanced `{...}` block."""
    depth = 0
    n = len(s)
    while i < n:
        c = s[i]
        if c in "([":
            depth += 1
        elif c in ")]":
            depth -= 1
        elif c == ";" and depth <= 0:
            return i + 1
        elif c == "{" and depth <= 0:
            return _match_brace(s, i)
        i += 1
    return n


class Split(NamedTuple):
    prod: str
    test: str
    test_mods: List[str]  # `#[cfg(test)] mod name;` declarations
    whole_file_test: bool


def split_test_code(stripped: str) -> Split:
    """Separate `#[cfg(test)]` items from production code (offsets preserved)."""
    if _INNER_CFG_TEST.search(stripped):
        return Split("", stripped, [], True)
    spans: List[Tuple[int, int]] = []
    test_mods: List[str] = []
    pos = 0
    while True:
        m = _CFG_TEST.search(stripped, pos)
        if not m:
            break
        start = m.start()
        i = _match_bracket(stripped, stripped.index("[", start))
        while True:
            j = i
            while j < len(stripped) and stripped[j].isspace():
                j += 1
            if stripped.startswith("#[", j):
                i = _match_bracket(stripped, j + 1)
                continue
            i = j
            break
        end = _item_end(stripped, i)
        spans.append((start, end))
        decl = re.match(
            r"(?:pub(?:\s*\([^)]*\))?\s+)?mod\s+(\w+)\s*;", stripped[i:end]
        )
        if decl:
            test_mods.append(decl.group(1))
        pos = max(end, start + 1)
    prod = list(stripped)
    test = [c if c == "\n" else " " for c in stripped]
    for a, b in spans:
        for k in range(a, b):
            if stripped[k] != "\n":
                prod[k] = " "
                test[k] = stripped[k]
    return Split("".join(prod), "".join(test), test_mods, False)


# --------------------------------------------------------------------------
# `use` trees

_USE_STMT = re.compile(r"\buse\s+([^;]+);")
_USE_TOK = re.compile(r"::|[{},*]|r#[A-Za-z_]\w*|[A-Za-z_]\w*")


def _drop_self(segs: List[str]) -> List[str]:
    return segs[:-1] if segs and segs[-1] == "self" else segs


def _parse_use(toks: List[str], pos: int, prefix: List[str]):
    out: List[Tuple[List[str], str]] = []
    segs = list(prefix)
    started = False
    while pos < len(toks):
        t = toks[pos]
        if t == "::":
            pos += 1
            continue
        if t == "{":
            pos += 1
            while pos < len(toks):
                sub, pos = _parse_use(toks, pos, segs)
                out.extend(sub)
                if pos < len(toks) and toks[pos] == ",":
                    pos += 1
                    continue
                break
            if pos < len(toks) and toks[pos] == "}":
                pos += 1
            return out, pos
        if t == "*":
            out.append((segs, "*"))
            return out, pos + 1
        if t == "as":
            bound = toks[pos + 1] if pos + 1 < len(toks) else "_"
            out.append((_drop_self(segs), bound))
            return out, pos + 2
        if t in (",", "}"):
            break
        segs.append(t)
        started = True
        pos += 1
    if started:
        segs = _drop_self(segs)
        out.append((segs, segs[-1] if segs else "_"))
    return out, pos


def expand_use(tree: str) -> List[Tuple[List[str], str]]:
    """Expand a use tree into (path segments, bound name); globs bind `*`."""
    out, _ = _parse_use(_USE_TOK.findall(tree), 0, [])
    return out


# --------------------------------------------------------------------------
# The module tree


class Target(NamedTuple):
    file: Optional[str]  # None for re-exported crates (aliases)
    item: str
    layer: str

    @property
    def key(self) -> str:
        return "%s::%s" % (self.file or "<alias>", self.item)


class FileInfo:
    def __init__(self, rel: str, module: Tuple[str, ...], text: str):
        self.rel = rel
        self.module = module
        stripped = strip_code(text)
        split = split_test_code(stripped)
        self.test_mods = split.test_mods
        self.is_test_file = split.whole_file_test or _is_test_name(rel)
        if self.is_test_file:
            self.prod = ""
            self.test = stripped
        else:
            self.prod = split.prod
            self.test = split.test
        self.layer = layer_of_path(rel)
        self.facade = not _DEFINES.search(self.prod) and not self.is_test_file
        # (absolute path segments, bound name) for each `use`; filled by Tree.
        self.bindings: List[Tuple[List[str], str]] = []


_DEFINES = re.compile(
    r"(?:^|[\s;{}])(?:pub(?:\s*\([^)]*\))?\s+)?(?:(?:async|const|unsafe|extern)\s+)*"
    r"(?:fn|struct|enum|trait|type|static|union|impl|macro_rules!)\b|"
    r"(?:^|[\s;{}])(?:pub(?:\s*\([^)]*\))?\s+)?const\s+[A-Za-z_]"
)


def _is_test_name(rel: str) -> bool:
    parts = rel.split("/")
    name = parts[-1]
    return (
        name in ("tests.rs", "test_support.rs", "test_utils.rs")
        or name.endswith("_tests.rs")
        or name.endswith("_test.rs")
        or "test_utils" in parts[:-1]
        or "testdata" in parts
    )


class Tree:
    def __init__(self, src: Path):
        self.src = src
        self.files: Dict[str, FileInfo] = {}
        self.modules: Dict[Tuple[str, ...], str] = {}
        for path in sorted(src.rglob("*.rs")):
            rel = path.relative_to(src).as_posix()
            module = _module_of(rel)
            self.modules[module] = rel
            self.files[rel] = FileInfo(
                rel, module, path.read_text(encoding="utf-8", errors="replace")
            )
        self._mark_test_modules()
        for info in self.files.values():
            self._collect_bindings(info)

    def _mark_test_modules(self) -> None:
        for info in list(self.files.values()):
            for name in info.test_mods:
                child = info.module + (name,)
                for module, rel in self.modules.items():
                    if module[: len(child)] == child and module:
                        fi = self.files[rel]
                        if not fi.is_test_file:
                            fi.is_test_file = True
                            fi.test = fi.test or fi.prod
                            fi.prod = ""
                            fi.facade = False

    def children(self, module: Tuple[str, ...]) -> Set[str]:
        n = len(module)
        return {m[n] for m in self.modules if len(m) == n + 1 and m[:n] == module}

    def to_abs(self, module: Tuple[str, ...], segs: List[str]) -> Optional[List[str]]:
        """Absolute path (without `crate`) or None for external paths."""
        if not segs:
            return None
        first = segs[0]
        if first == "crate":
            return list(segs[1:])
        if first in ("self", "super"):
            base = list(module)
            i = 0
            while i < len(segs) and segs[i] in ("self", "super"):
                if segs[i] == "super":
                    base = base[:-1]
                i += 1
            return base + list(segs[i:])
        if module + (first,) in self.modules:
            return list(module) + list(segs)
        return None

    def _collect_bindings(self, info: FileInfo) -> None:
        for m in _USE_STMT.finditer(info.prod):
            for segs, bound in expand_use(m.group(1)):
                absolute = self.to_abs(info.module, segs)
                if absolute is not None:
                    info.bindings.append((absolute, bound))

    # -- resolution ------------------------------------------------------

    def resolve(self, segs: List[str], depth: int = 0) -> Optional[Target]:
        if depth > 10 or not segs:
            return None
        for alias, layer in ALIASES.items():
            if tuple(segs[: len(alias)]) == alias:
                rest = segs[len(alias) :]
                return Target(None, "::".join(alias) + ("::" + rest[0] if rest else ""), layer)
        module: Tuple[str, ...] = ()
        i = 0
        while i < len(segs) and module + (segs[i],) in self.modules:
            module += (segs[i],)
            i += 1
        rel = self.modules.get(module)
        if rel is None:
            return None
        if i == len(segs):
            return Target(rel, "", self.files[rel].layer)
        target = self._resolve_item(module, rel, segs[i], depth)
        # A re-exported module (`pub use crate::core_staging::protocol;`): keep
        # resolving the remaining segments inside the module it points to.
        if target is not None and not target.item and target.file and i + 1 < len(segs):
            return self.resolve(list(self.files[target.file].module) + segs[i + 1 :], depth + 1)
        # An inline `mod name { ... }` is a namespace, not an item: count the
        # item inside it (`genai::shell_executable`, not `genai`).
        if (
            target is not None
            and target.file == rel
            and i + 1 < len(segs)
            and re.search(r"\bmod\s+%s\s*\{" % re.escape(segs[i]), self.files[rel].prod)
        ):
            return Target(rel, "%s::%s" % (segs[i], segs[i + 1]), target.layer)
        return target

    def _defines(self, info: FileInfo, item: str) -> bool:
        pattern = r"\b(?:fn|struct|enum|trait|type|const|static|union|mod|macro_rules!)\s+%s\b" % re.escape(item)
        return bool(re.search(pattern, info.prod))

    def _has_item(self, rel: str, item: str, depth: int) -> bool:
        info = self.files[rel]
        if self._defines(info, item):
            return True
        for absolute, bound in info.bindings:
            if bound == item:
                return True
            if bound == "*" and depth < 6:
                target = self.modules.get(tuple(absolute))
                if target and self._has_item(target, item, depth + 1):
                    return True
        return False

    def _resolve_item(
        self, module: Tuple[str, ...], rel: str, item: str, depth: int
    ) -> Optional[Target]:
        info = self.files[rel]
        for absolute, bound in info.bindings:
            if bound == item:
                return self.resolve(absolute, depth + 1)
        if self._defines(info, item):
            return Target(rel, item, info.layer)
        for absolute, bound in info.bindings:
            if bound == "*":
                target = self.modules.get(tuple(absolute))
                if target and self._has_item(target, item, depth + 1):
                    return self.resolve(absolute + [item], depth + 1)
        # A glob re-export of the core crate (`pub use crate::core_staging::x::*`):
        # the item is not defined here, so it comes from there.
        for absolute, bound in info.bindings:
            if bound == "*":
                for alias, layer in ALIASES.items():
                    if tuple(absolute[: len(alias)]) == alias:
                        return Target(None, item, layer)
        if info.facade:
            return None
        return Target(rel, item, info.layer)

    # -- references ------------------------------------------------------

    def references(self, info: FileInfo, text: str) -> List[Tuple[Target, int]]:
        """Every resolved reference in `text` with its offset."""
        refs: List[Tuple[Target, int]] = []
        blanked = list(text)
        module_names: Dict[str, List[str]] = {}
        for m in _USE_STMT.finditer(text):
            for segs, bound in expand_use(m.group(1)):
                absolute = self.to_abs(info.module, segs)
                if absolute is None:
                    continue
                if bound == "*":
                    target = self.resolve(absolute)
                    if target and target.file and not self.files[target.file].facade:
                        refs.append((Target(target.file, "*", target.layer), m.start()))
                    continue
                if tuple(absolute) in self.modules or any(
                    absolute[: len(a)] == list(a) for a in ALIASES
                ) and len(absolute) <= max(len(a) for a in ALIASES):
                    module_names[bound] = absolute
                target = self.resolve(absolute)
                if target and target.item:
                    refs.append((target, m.start()))
            for k in range(m.start(), m.end()):
                if blanked[k] != "\n":
                    blanked[k] = " "
        code = "".join(blanked)
        for m in re.finditer(
            r"(?<![\w:.])((?:crate|super|self)(?:\s*::\s*[A-Za-z_]\w*)+)", code
        ):
            segs = [s.strip() for s in m.group(1).split("::")]
            absolute = self.to_abs(info.module, segs)
            target = self.resolve(absolute) if absolute is not None else None
            if target and target.item:
                refs.append((target, m.start()))
        if module_names:
            names = "|".join(re.escape(n) for n in sorted(module_names, key=len, reverse=True))
            for m in re.finditer(r"(?<![\w:.])(%s)((?:\s*::\s*[A-Za-z_]\w*)+)" % names, code):
                rest = [s.strip() for s in m.group(2).split("::") if s.strip()]
                target = self.resolve(module_names[m.group(1)] + rest)
                if target and target.item:
                    refs.append((target, m.start()))
        return refs


def _module_of(rel: str) -> Tuple[str, ...]:
    parts = rel[:-3].split("/")
    if parts[-1] in ("mod", "lib"):
        parts = parts[:-1]
    return tuple(parts)


# --------------------------------------------------------------------------
# Analysis


class Site(NamedTuple):
    file: str
    line: int


class Report:
    def __init__(self) -> None:
        # pair -> item key -> sites
        self.prod: Dict[str, Dict[str, List[Site]]] = defaultdict(lambda: defaultdict(list))
        self.test: Dict[str, Dict[str, List[Site]]] = defaultdict(lambda: defaultdict(list))
        self.sqlite_mentions: List[Site] = []
        self.core_forbidden: List[Tuple[str, int, str]] = []
        self.layer_files: Dict[str, int] = defaultdict(int)

    def counts(self, which: str = "prod") -> Dict[str, int]:
        data = self.prod if which == "prod" else self.test
        return {pair: len(items) for pair, items in data.items()}

    def sites(self, which: str = "prod") -> Dict[str, int]:
        data = self.prod if which == "prod" else self.test
        return {pair: sum(len(s) for s in items.values()) for pair, items in data.items()}

    def distinct_items(self, which: str = "prod") -> int:
        data = self.prod if which == "prod" else self.test
        return len({key for items in data.values() for key in items})


def _line_of(text: str, offset: int) -> int:
    return text.count("\n", 0, offset) + 1


def analyze(src: Path) -> Report:
    tree = Tree(src)
    report = Report()
    for info in tree.files.values():
        report.layer_files[info.layer] += 1
        if any(info.rel.startswith(d) for d in SQLITE_FREE_DIRS) and not info.is_test_file:
            for m in SQLITE_MENTION.finditer(info.prod):
                report.sqlite_mentions.append(Site(info.rel, _line_of(info.prod, m.start())))
        if info.layer == "core" and not info.is_test_file:
            for lineno, line in enumerate(info.prod.split("\n"), start=1):
                if line.lstrip().startswith("//"):
                    continue
                for m in CORE_FORBIDDEN.finditer(line):
                    report.core_forbidden.append((info.rel, lineno, m.group(1)))
        if info.facade:
            continue
        for text, is_test in ((info.prod, False), (info.test, True)):
            if not text.strip():
                continue
            allowed = ALLOWED_WITH_DEV[info.layer] if is_test else ALLOWED[info.layer]
            for target, offset in tree.references(info, text):
                if target.layer == info.layer or target.layer in allowed:
                    continue
                pair = "%s->%s" % (info.layer, target.layer)
                bucket = report.test if is_test else report.prod
                bucket[pair][target.key].append(Site(info.rel, _line_of(text, offset)))
    return report


# --------------------------------------------------------------------------
# CLI


def pair_order(pair: str):
    a, b = pair.split("->")
    return (LAYERS.index(a), LAYERS.index(b))


def load_allowance(path: Path) -> Dict[str, int]:
    if not path.exists():
        return {}
    data = json.loads(path.read_text())
    out = dict(data.get("pairs", {}))
    out["core_staging_sqlite_mentions"] = data.get("core_staging_sqlite_mentions", 0)
    out["core_forbidden_mentions"] = data.get("core_forbidden_mentions", 0)
    return out


def current_counts(report: Report) -> Dict[str, int]:
    counts = dict(report.counts())
    counts["core_staging_sqlite_mentions"] = len(report.sqlite_mentions)
    counts["core_forbidden_mentions"] = len(report.core_forbidden)
    return counts


def print_table(title: str, counts: Dict[str, int], sites: Dict[str, int], allowance: Optional[Dict[str, int]]) -> None:
    print(title)
    header = "  %-22s %6s %6s" % ("pair", "items", "sites")
    if allowance is not None:
        header += " %9s" % "allowance"
    print(header)
    for pair in sorted(counts, key=pair_order):
        line = "  %-22s %6d %6d" % (pair, counts[pair], sites.get(pair, 0))
        if allowance is not None:
            line += " %9d" % allowance.get(pair, 0)
        print(line)


def main(argv: Optional[Iterable[str]] = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--src", type=Path, default=DEFAULT_SRC, help="source root (default: crates/agent/src)")
    ap.add_argument("--allowance", type=Path, default=DEFAULT_ALLOWANCE)
    ap.add_argument("--verbose", "-v", action="store_true", help="list every upward item with its sites")
    ap.add_argument("--tests", action="store_true", help="also report test-only upward edges (informational)")
    ap.add_argument("--layers", action="store_true", help="print the number of files per layer")
    ap.add_argument("--json", action="store_true", help="print the counts as JSON")
    ap.add_argument("--write-allowance", action="store_true", help="write the current counts as the allowance")
    ap.add_argument("--allow-increase", action="store_true", help="with --write-allowance: permit raising a count")
    args = ap.parse_args(list(argv) if argv is not None else None)

    if not args.src.is_dir():
        print("error: %s is not a directory" % args.src, file=sys.stderr)
        return 2

    report = analyze(args.src)
    counts = current_counts(report)
    allowance = load_allowance(args.allowance)

    if args.write_allowance:
        increases = [p for p, n in counts.items() if n > allowance.get(p, 0)]
        if increases and not args.allow_increase and args.allowance.exists():
            print("refusing to raise the allowance for: %s (use --allow-increase)" % ", ".join(sorted(increases)), file=sys.stderr)
            return 1
        pairs = {p: n for p, n in sorted(counts.items(), key=lambda kv: pair_order(kv[0]) if "->" in kv[0] else (99, 99)) if "->" in p}
        payload = {
            "_comment": "Distinct upward production items per layer pair, from scripts/check-agent-layering.py. Lower these as cuts land; never raise.",
            "pairs": pairs,
            "core_staging_sqlite_mentions": counts["core_staging_sqlite_mentions"],
            "core_forbidden_mentions": counts["core_forbidden_mentions"],
        }
        args.allowance.write_text(json.dumps(payload, indent=2) + "\n")
        print("wrote %s" % args.allowance)
        return 0

    if args.json:
        print(json.dumps({"production": {p: {"items": n, "sites": report.sites()[p]} for p, n in report.counts().items()}, "total_items": report.distinct_items(), "core_staging_sqlite_mentions": len(report.sqlite_mentions)}, indent=2, sort_keys=True))
    else:
        if args.layers:
            print("files per layer (crates/agent/src):")
            for layer in LAYERS:
                print("  %-10s %4d" % (layer, report.layer_files.get(layer, 0)))
        print_table("production upward references (distinct items per layer pair):", report.counts(), report.sites(), allowance)
        print(
            "  total: %d (item, layer pair) entries, %d distinct items"
            % (sum(report.counts().values()), report.distinct_items())
        )
        if report.sqlite_mentions:
            print("SQLite mentioned in core_staging/: %d site(s)" % len(report.sqlite_mentions))
            for site in report.sqlite_mentions[:20]:
                print("    %s:%d" % site)
        if report.core_forbidden:
            print("rmcp, querymt_utils or rusqlite used in core files: %d site(s)" % len(report.core_forbidden))
            for rel, lineno, name in report.core_forbidden[:20]:
                print("    %s:%d  %s" % (rel, lineno, name))
        if args.verbose:
            for pair in sorted(report.prod, key=pair_order):
                print("\n%s" % pair)
                for key in sorted(report.prod[pair]):
                    sites = report.prod[pair][key]
                    shown = ", ".join("%s:%d" % s for s in sites[:3])
                    more = " (+%d)" % (len(sites) - 3) if len(sites) > 3 else ""
                    print("  %s  <- %s%s" % (key, shown, more))
        if args.tests:
            print()
            print_table("test-only upward references beyond the dev edges (informational):", report.counts("test"), report.sites("test"), None)

    failures = []
    for pair, n in counts.items():
        limit = allowance.get(pair, 0)
        if n > limit:
            failures.append("%s: %d > allowance %d" % (pair, n, limit))
    if failures:
        print("\nlayering check FAILED:", file=sys.stderr)
        for failure in failures:
            print("  " + failure, file=sys.stderr)
        print("Remove the new upward reference (move the definition down, or invert the dependency).", file=sys.stderr)
        return 1
    lowered = [p for p, n in counts.items() if n < allowance.get(p, 0)]
    if lowered and not args.json:
        print("\nbelow the allowance for %s: lower it with --write-allowance." % ", ".join(sorted(lowered)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
