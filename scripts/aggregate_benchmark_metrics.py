#!/usr/bin/env python3
"""Aggregate repeated Membrick retrieval benchmark reports.

Reports must use the same benchmark configuration and contain the same query
IDs. Metrics are averaged across repetitions (not pooled across questions), so
repeated runs of one dataset report run-to-run stability rather than inflating
the sample size. Output is a new JSON file and source reports are unchanged.

Usage:
  python3 scripts/aggregate_benchmark_metrics.py run-1.json run-2.json \
      --out comparison/results/aggregate.json
"""

from __future__ import annotations

import argparse
import json
import math
import statistics
import sys
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
CONFIG_KEYS = (
    "ner_provider", "index_mode", "structured_fact_retrieval", "combsum",
    "rrf", "prf", "rm3", "bo1", "text_rerank_ngram", "text_rerank_lcs",
)
SCALAR_AGGREGATE_KEYS = ("mrr", "ndcg_at_10", "analysis_ms")


def summarize(values: list[float]) -> dict[str, float | int]:
    if not values:
        raise ValueError("cannot summarize an empty metric")
    return {
        "mean": statistics.fmean(values),
        "stdev": statistics.pstdev(values),
        "min": min(values),
        "max": max(values),
        "runs": len(values),
    }


def numeric_series(reports: list[dict[str, Any]], getter) -> dict[str, Any]:
    values = [getter(report) for report in reports]
    if any(not isinstance(value, (int, float)) or not math.isfinite(value) for value in values):
        raise ValueError("required metric is missing or non-numeric")
    return summarize([float(value) for value in values])


def aggregate_reports(paths: list[Path]) -> dict[str, Any]:
    if len(paths) < 2:
        raise ValueError("provide at least two reports to aggregate")
    reports = [json.loads(path.read_text()) for path in paths]
    config = {key: reports[0].get(key) for key in CONFIG_KEYS}
    def query_index(report: dict[str, Any], path: Path) -> dict[str, dict[str, Any]]:
        rows = report.get("per_query", [])
        indexed = {str(row["id"]): row for row in rows}
        if not rows or len(indexed) != len(rows):
            raise ValueError(f"{path} has no per_query records or contains duplicate question IDs")
        return indexed

    baseline_rows = query_index(reports[0], paths[0])
    baseline_ids = set(baseline_rows)
    if not baseline_ids:
        raise ValueError(f"{paths[0]} has no per_query records")
    for path, report in zip(paths[1:], reports[1:]):
        if {key: report.get(key) for key in CONFIG_KEYS} != config:
            raise ValueError(f"benchmark configuration differs in {path}")
        indexed = query_index(report, path)
        query_ids = set(indexed)
        if query_ids != baseline_ids:
            raise ValueError(f"question IDs differ in {path}")
        if {key: row.get("question_type") for key, row in indexed.items()} != {
            key: row.get("question_type") for key, row in baseline_rows.items()
        }:
            raise ValueError(f"question type assignments differ in {path}")

    aggregate: dict[str, Any] = {"query_count": len(baseline_ids)}
    for key in SCALAR_AGGREGATE_KEYS:
        aggregate[key] = numeric_series(reports, lambda report, k=key: report["aggregate"][k])
    for metric in ("recall_at_k", "recall_any_at_k"):
        ks = sorted(reports[0]["aggregate"][metric], key=int)
        aggregate[metric] = {
            k: numeric_series(reports, lambda report, m=metric, rank=k: report["aggregate"][m][rank])
            for k in ks
        }
    timing_keys = sorted(set.intersection(*[
        set(report["aggregate"].get("timings", {})) for report in reports
    ]))
    aggregate["timings_ms"] = {
        key: numeric_series(reports, lambda report, name=key: report["aggregate"]["timings"][name])
        for key in timing_keys
    }

    question_types = sorted(reports[0].get("by_question_type", {}))
    by_type: dict[str, Any] = {}
    for question_type in question_types:
        by_type[question_type] = {"query_count": reports[0]["by_question_type"][question_type]["query_count"]}
        for key in SCALAR_AGGREGATE_KEYS:
            if key in reports[0]["by_question_type"][question_type]:
                by_type[question_type][key] = numeric_series(
                    reports, lambda report, q=question_type, metric=key: report["by_question_type"][q][metric]
                )
        for metric in ("recall_at_k", "recall_any_at_k"):
            ks = sorted(reports[0]["by_question_type"][question_type].get(metric, {}), key=int)
            by_type[question_type][metric] = {
                k: numeric_series(
                    reports,
                    lambda report, q=question_type, m=metric, rank=k: report["by_question_type"][q][m][rank],
                )
                for k in ks
            }

    query_rows = [query_index(report, path) for report, path in zip(reports, paths)]
    per_query = []
    for query_id in sorted(baseline_ids):
        first = query_rows[0][query_id]
        row: dict[str, Any] = {"id": query_id, "question_type": first.get("question_type")}
        for metric in ("mrr", "ndcg_at_10"):
            row[metric] = summarize([float(items[query_id][metric]) for items in query_rows])
        for metric in ("recall_at_k", "recall_any_at_k"):
            ks = sorted(first.get(metric, {}), key=int)
            row[metric] = {
                k: summarize([float(items[query_id][metric][k]) for items in query_rows])
                for k in ks
            }
        per_query.append(row)

    return {
        "schema_version": 1,
        "aggregation": "repeated_runs",
        "run_count": len(reports),
        "configuration": config,
        "source_reports": [path.resolve().relative_to(ROOT).as_posix() for path in paths],
        "aggregate": aggregate,
        "by_question_type": by_type,
        "per_query": per_query,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("reports", nargs="+", help="same-dataset benchmark report JSON files")
    parser.add_argument("--out", required=True, help="new aggregate JSON output path")
    args = parser.parse_args()
    try:
        paths = []
        for raw in args.reports:
            path = Path(raw)
            if not path.is_absolute():
                path = ROOT / path
            path = path.resolve()
            path.relative_to(ROOT)
            paths.append(path)
        output = Path(args.out)
        if not output.is_absolute():
            output = ROOT / output
        output = output.resolve()
        output.relative_to(ROOT)
        result = aggregate_reports(paths)
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    except (OSError, KeyError, TypeError, ValueError, json.JSONDecodeError) as error:
        print(f"benchmark aggregation failed: {error}", file=sys.stderr)
        return 1
    print(f"Aggregated {result['run_count']} reports ({result['aggregate']['query_count']} matched questions) into {output.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
