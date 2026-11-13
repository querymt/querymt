#!/usr/bin/env python3
"""Blind bundle of final answers from scripts/kb-measure.sh, for judging.

  kb-blind.py OUT_DIR [-g GROUP]            write OUT_DIR/blind/bundle.md and key.json
  kb-blind.py OUT_DIR --reveal scores.csv   join judged scores with the key

The bundle holds every final answer under a random id, grouped by prompt, in
random order, with the arm hidden. The key (id -> prompt, arm, rep) is written
to a separate file that the judge must not open until the scores are saved.

scores.csv has the header `id,facts,errors`:
  facts   how many rubric facts the answer states correctly
  errors  how many statements in it are wrong against the source

Redaction: lines that name the feature itself (repo_knowledge_* tools, "knowledge
note/doc") are replaced with `[redacted]`, because they would reveal the arm. An
answer can still betray its arm by style, so treat the blindness as partial.
"""
import csv
import json
import random
import re
import statistics as st
import sys
from collections import defaultdict
from pathlib import Path

LEAK = re.compile(
    r"repo_knowledge|repository[- ]knowledge|knowledge (?:note|doc|map)|"
    r"(?:the )?(?:repo|repository) (?:note|doc)s?\b|crates/agent/knowledge/|"
    r"marked\s+\"?unverified|docs section", re.I)


def final_answer(log: str) -> str:
    parts = re.split(r"\n\[ASSISTANT\] ", log)
    return parts[-1].split("\nAgent:")[0].strip()


def redact(text: str) -> str:
    return "\n".join("[redacted]" if LEAK.search(l) else l for l in text.split("\n"))


def build(out: Path, group):
    rows = list(csv.DictReader(open(out / "results.csv")))
    items = []
    for r in rows:
        if group and r["group"] != group:
            continue
        log = out / f"{r['prompt_id']}-{r['arm']}-{r['rep']}.log"
        items.append({"prompt_id": r["prompt_id"], "arm": r["arm"], "rep": r["rep"],
                      "answer": redact(final_answer(log.read_text()))})
    rng = random.Random()
    by_prompt = defaultdict(list)
    for it in items:
        by_prompt[it["prompt_id"]].append(it)
    key, md, n = {}, ["# Blind answers\n"], 0
    for pid in sorted(by_prompt):
        md.append(f"\n---\n\n# Prompt {pid}\n")
        group_items = by_prompt[pid]
        rng.shuffle(group_items)
        for it in group_items:
            n += 1
            bid = f"{pid}-{n:03d}"
            key[bid] = {k: it[k] for k in ("prompt_id", "arm", "rep")}
            md.append(f"\n## {bid}\n\n{it['answer']}\n")
    d = out / "blind"
    d.mkdir(exist_ok=True)
    (d / "bundle.md").write_text("\n".join(md))
    (d / "key.json").write_text(json.dumps(key, indent=1))
    print(f"{len(key)} answers -> {d}/bundle.md (key: {d}/key.json, do not open before scoring)")


def reveal(out: Path, scores: Path):
    key = json.load(open(out / "blind" / "key.json"))
    per = defaultdict(lambda: defaultdict(list))
    for r in csv.DictReader(open(scores)):
        k = key[r["id"]]
        per[k["prompt_id"]][k["arm"]].append((int(r["facts"]), int(r["errors"])))
    print(f"{'prompt':>8} {'arm':>4} {'n':>3} {'facts mean':>11} {'errors mean':>12}")
    tot = defaultdict(lambda: ([], []))
    for pid in sorted(per):
        for arm in ("on", "off"):
            v = per[pid].get(arm, [])
            if not v:
                continue
            f = [a for a, _ in v]
            e = [b for _, b in v]
            tot[arm][0].extend(f)
            tot[arm][1].extend(e)
            print(f"{pid:>8} {arm:>4} {len(v):>3} {st.mean(f):>11.2f} {st.mean(e):>12.2f}")
    for arm in ("on", "off"):
        f, e = tot[arm]
        if f:
            print(f"{'ALL':>8} {arm:>4} {len(f):>3} {st.mean(f):>11.2f} {st.mean(e):>12.2f}")


def main():
    a = sys.argv[1:]
    if not a:
        sys.exit(__doc__)
    out = Path(a[0])
    if "--reveal" in a:
        reveal(out, Path(a[a.index("--reveal") + 1]))
    else:
        build(out, a[a.index("-g") + 1] if "-g" in a else None)


if __name__ == "__main__":
    main()
