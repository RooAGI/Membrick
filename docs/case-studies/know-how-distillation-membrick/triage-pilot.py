#!/usr/bin/env python3
"""Recalculate and validate metrics in the saved Membrick triage pilot."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import sys
from collections import defaultdict


DEFAULT_LEDGER = Path(__file__).with_name("triage-pilot-2026-11-16.json")


def calculate(rows: list[dict[str, str]]) -> dict[str, dict[str, int | float]]:
    counts: dict[str, dict[str, int]] = defaultdict(
        lambda: {"tp": 0, "fp": 0, "fn": 0, "tn": 0}
    )
    for row in rows:
        predicted_positive = row["triage"] == "selected"
        actual_positive = row["label"] == "relevant"
        if predicted_positive and actual_positive:
            key = "tp"
        elif predicted_positive:
            key = "fp"
        elif actual_positive:
            key = "fn"
        else:
            key = "tn"
        counts[row["topic"]][key] += 1

    result: dict[str, dict[str, int | float]] = {}
    for topic, c in counts.items():
        denom = 2 * c["tp"] + c["fp"] + c["fn"]
        result[topic] = {**c, "f1": 2 * c["tp"] / denom if denom else 0.0}
    return result


def hash_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("ledger", nargs="?", type=Path, default=DEFAULT_LEDGER)
    parser.add_argument(
        "--source-root",
        type=Path,
        help="also verify each recorded raw source file and SHA-256 under this directory",
    )
    args = parser.parse_args()

    try:
        ledger = json.loads(args.ledger.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        print(f"Cannot read ledger {args.ledger}: {exc}", file=sys.stderr)
        return 2

    rows = ledger["audit"]
    pools = ledger["candidate_pools"]
    for row in rows:
        topic_pool = pools[row["topic"]]
        pool = topic_pool[row["triage"]]
        if row["thread_id"] not in pool:
            print(
                f"Audit thread {row['thread_id']} is absent from its {row['triage']} pool",
                file=sys.stderr,
            )
            return 1

    per_topic = calculate(rows)
    total = {key: sum(c[key] for c in per_topic.values()) for key in ("tp", "fp", "fn", "tn")}
    denominator = 2 * total["tp"] + total["fp"] + total["fn"]
    micro_f1 = 2 * total["tp"] / denominator if denominator else 0.0
    precision_denom = total["tp"] + total["fp"]
    recall_denom = total["tp"] + total["fn"]
    precision = total["tp"] / precision_denom if precision_denom else 0.0
    recall = total["tp"] / recall_denom if recall_denom else 0.0
    macro_f1 = sum(float(c["f1"]) for c in per_topic.values()) / len(per_topic)

    print("Per-topic results:")
    for topic, c in per_topic.items():
        print(
            f"  {topic}: TP={c['tp']} FP={c['fp']} FN={c['fn']} TN={c['tn']} "
            f"F1={float(c['f1']):.4f}"
        )
    print(
        f"Pooled: TP={total['tp']} FP={total['fp']} FN={total['fn']} TN={total['tn']} "
        f"precision={precision:.4f} recall={recall:.4f} micro-F1={micro_f1:.4f}"
    )
    print(f"Macro-F1: {macro_f1:.4f}")

    recorded = ledger["metrics"]
    if abs(macro_f1 - recorded["macro_f1"]) > 0.0001:
        print("Calculated macro-F1 differs from the ledger", file=sys.stderr)
        return 1
    if abs(micro_f1 - recorded["pooled"]["micro_f1"]) > 0.0001:
        print("Calculated micro-F1 differs from the ledger", file=sys.stderr)
        return 1

    if args.source_root:
        mismatches = 0
        unchecked = 0
        for row in rows:
            source_path = args.source_root / row["source"]
            if not source_path.is_file():
                print(f"Missing source: {source_path}", file=sys.stderr)
                mismatches += 1
                continue
            actual_hash = hash_file(source_path)
            expected_hash = row.get("local_file_sha256", row.get("sha256"))
            if expected_hash is None:
                print(f"No local SHA-256 recorded; skipped: {source_path}")
                unchecked += 1
                continue
            if actual_hash != expected_hash:
                print(f"Hash mismatch: {source_path}", file=sys.stderr)
                mismatches += 1
        if mismatches:
            return 1
        if unchecked:
            print(f"Verified available local hashes; {unchecked} source hash(es) were not recorded.")
        else:
            print("All recorded local source hashes match.")

    print("Ledger pools and recorded metric totals are consistent.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
