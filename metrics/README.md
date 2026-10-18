# Metrics

This directory defines metrics for recorded agent sessions. Retrieval benchmark
aggregation is provided separately by
[`scripts/aggregate_benchmark_metrics.py`](../scripts/aggregate_benchmark_metrics.py).

## Session metrics

This directory defines the user-facing metric report for recorded Claude and
Codex sessions.

## Files

- `session-metric-report-design.md` — metric definitions and review UI contract
- `session-metric-report.schema.json` — JSON Schema for generated reports
- `generate_session_metric_report.py` — CLI parser and report generator
- `reports/` — generated report instances; session archives remain the source evidence

Reports support both a single recorded session and a baseline/replay
comparison. Quality and efficiency are intentionally reported separately: a
faster session is not an improvement if it fails the task.

The CLI reads `manifest.json` and `events.jsonl`. It tolerates incomplete or
malformed JSONL lines, preserves unavailable measurements as `null`, and does
not modify the recorded session.

## Generate a report

The report generator should consume immutable session manifests and events,
then write a new report revision. It must not modify the session archive.

```text
metrics/
├── session-metric-report-design.md
├── session-metric-report.schema.json
└── reports/
    └── <report-id>.json
```

Missing provider measurements are represented as `null`, never as zero. This
distinguishes “not observed” from “observed as zero.”

## Aggregate repeated retrieval runs

Use the aggregation script when comparing repeated LongMemEval or compatible
retrieval benchmark reports. It requires identical benchmark settings and the
same set of question IDs, then reports mean, population standard deviation,
minimum, and maximum across runs for aggregate metrics, question types, and
each question. This makes run-to-run variation visible without treating
repetitions as additional questions. The source JSON files are not modified.

```bash
python3 scripts/aggregate_benchmark_metrics.py \
  comparison/results/run-1.json \
  comparison/results/run-2.json \
  comparison/results/run-3.json \
  --out comparison/results/repeated-runs.json
```

The generated JSON preserves the shared configuration and source report paths.
It is an analysis artifact; it does not update the website or overwrite the
single-run headline.
