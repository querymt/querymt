#!/usr/bin/env python3
"""Score a blind bundle (scripts/kb-blind.py) against a fixed fact rubric.

  kb-rubric.py OUT_DIR            reads OUT_DIR/blind/bundle.md, writes OUT_DIR/blind/scores.csv

It never reads key.json, so the arm is unknown while scoring. Each rubric item
is a regex for one fact that exists in the source and that a complete answer to
the prompt should state. This measures coverage (presence of a fact), not
correctness: a wrong statement still scores if it contains the keyword, so the
`errors` column is left at 0 and must be filled by a human judge if wanted.
Rubrics are keyed by prompt id (p01..p05, the concept group in
scripts/kb-measure-prompts.txt) and were written by one person after reading a
few earlier answers; treat them as one reasonable checklist, not ground truth.
"""
import csv
import re
import sys
from pathlib import Path

RUBRIC = {
    "p01": ["is_retryable", "call_with_retry", "StreamRetryBudget|handle_stream_failure",
            "max_stream_retries", "jitter", "retry_after|hint", "select!|cancel",
            "LlmRetryWait", "StreamRecovering", "duplicate",
            "ConnectionClosed|completion marker", "300"],
    "p02": ["NFKC", "2 KiB", "64", "STOPWORDS|stopword", "qualified identifier",
            "METADATA_THRESHOLD|at least 2|two distinct", "BODY_THRESHOLD|at least 3|three",
            "BM25|rank", "entrypoint", "lifecycle|draft", "budget|600"],
    "p03": ["plan_waves", "FileAccess", "ParallelSafe", "barrier", r"conflicts\(\)|paths_overlap",
            "join_all", "model order", "ReadScope|observes_write", "Unknown",
            "parallel_edit_guard", "mutation gate|mutation_gate", "file_access_batch"],
    "p04": ["normalize_arguments", r"Walker::decode", "JsonString", "ScalarToArray",
            r"plain\(\)", "anyOf", "validate_tool_arguments", "clone", "[Ii]dempotent",
            r"Repair \{", "updated_input", "ArgConflict", "raw|original"],
    "p05": ["ContextMiddleware", "compact_at_percent", "85", "/compact", "PreCompaction",
            "custom_summary", "mark_automatic_compaction", "PriceLimit",
            "create_compaction_messages", "[Pp]runing|prune", "PostCompaction",
            "fallback_max_tokens"],
}


def main():
    out = Path(sys.argv[1]) / "blind"
    text = (out / "bundle.md").read_text()
    rows = []
    # An answer ends at the next bundle id or prompt header, not at its own
    # `## ` sub-headings.
    for m in re.finditer(r"^## (p\d+-\d+)\n(.*?)(?=^## p\d+-\d+\n|^# Prompt |\Z)", text, re.S | re.M):
        bid, ans = m.group(1), m.group(2)
        pid = bid.split("-")[0]
        if pid not in RUBRIC:
            continue
        facts = sum(1 for k in RUBRIC[pid] if re.search(k, ans))
        rows.append((bid, facts, 0, len(RUBRIC[pid])))
    with open(out / "scores.csv", "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["id", "facts", "errors", "of"])
        w.writerows(rows)
    print(f"scored {len(rows)} answers without the key -> {out}/scores.csv")


if __name__ == "__main__":
    main()
