# Freshness ranking implementation plan

Status: CurrentState-only relative freshness is the default server and pipeline policy, following the user's explicit preference for fresher current-state results. `legacy` remains available as a settings override. The default accepts a small LongMemEval MRR/NDCG tradeoff for a repeatable Any-hit@3 gain. Freshness-specific labeled judgments and several production-path validations remain open; this choice does not mean every evaluation gate below has passed.

## Objective

Return relevant, applicable memories before outdated alternatives, while preserving historical records and durable knowledge. Freshness should help distinguish relevant candidates. It must not make an unrelated recent document useful or make a newer assertion authoritative.

All production entry points continue through `MemoryService`. Reuse existing query intent, temporal resolution, source documents, claims, semantic relations, score breakdowns, configuration, and evaluation tools. No separate memory store, ranking service, automatic deletion, or new environment variables are required.

## Current implementation and gaps

| Component | Current behavior | Implementation status |
|---|---|---|
| `src/temporal.rs` | Retains `recency_boost()` for compatibility | Adds validated `FreshnessPolicy` (`relative`, `legacy`, `disabled`) and a bounded, score-relative exponential delta. Missing, malformed, or non-finite evidence scores receive no delta. |
| `src/index/query.rs` | Scores retrieved candidates | Retains legacy scoring for comparison. Relative freshness is intentionally omitted at this stage to prevent it being applied before later retrieval and reranking stages. |
| `src/query_plan.rs` and `src/index/model.rs` | Query intent and temporal resolution already exist | Carry one resolved reference date and intent through single and segmented index queries; MemoryService reads the configured policy once per request. |
| `src/pipeline/config.rs`, `src/bin/server.rs` | Pipeline/server configuration exists | Add a typed policy to `PipelineOptions` and the project `server.toml` `[freshness]` table. Invalid values fail startup with a setting-specific error. |
| `src/memory_api.rs` | Owns final result assembly for production searches | Expands eligible relative-freshness candidate depth, merges structured and lexical results, applies freshness once after service reranking, preserves structured-first ordering, then truncates to `top_k`. |
| `src/segments/query.rs` | Merges independently ranked segment results | Returns ordinary candidates to MemoryService; the same final service stage applies freshness after segment merging. Cross-segment coverage and latency still require evaluation. |
| `src/engine/query_cache.rs` | Persists index snapshots, not final search rankings | Freshness is applied at query time; it does not affect the index cache key or require reindexing. |

The service still has independent ordinary, segment, conversational, and structured ranking stages. Relative freshness now runs once after these paths merge and after session-tag reranking. It preserves structured-first priority and sorts by adjusted score only within the structured and lexical tiers, because their score scales are not assumed comparable. Relative mode requests a deeper candidate pool (up to the existing `RERANK_DEEP_TOP_K`) before final ordering; candidate coverage and the resulting latency cost remain evaluation gates.

## Implemented policy and configuration

Relative freshness is the default. This optional project `.membrick/server.toml` setting makes the policy explicit:

```toml
[freshness]
mode = "relative"
half_life_days = 30.0
weight = 0.25
```

Only `CurrentState` currently receives the configured `weight`. `Lookup`, `Explain`, comparison, aggregation, choice, and temporal intents receive zero. This is deliberately conservative: a newer document is not assumed to be better for ordinary factual lookup or durable explanations. The delta is `baseline_score × effective_weight × 2^(-age_days / half_life_days)`. Future timestamps are treated as age zero. This affects search ordering only; it does not mutate timestamps or evidence.

`legacy` retains the previous absolute 30-day half-life/0.25 recency boost. `disabled` turns generic freshness off. Omit `[freshness]` to use CurrentState-only relative freshness. The selected policy is read when the server starts; changing it requires restarting the server, but no index rebuild.

Implemented deterministic checks cover formula scaling/decay, malformed and missing dates, future dates, intent selection, invalid configuration values, and ranking with a single request reference date.

The current 0.25 maximum is an absolute score increment. Observed MCP results have scores in the hundreds, so it may have little ranking influence. This is a calibration hypothesis, not proof that every irrelevant result is caused by recency.

See [temporal resolution](temporal-query-resolution.md), [semantic supersession](semantic-supersession.md), and [document enrichment](document-enrichment-plan.md) for related contracts.

## Intended query flow

```text
MCP / hooks / HTTP / supported library APIs
                    ↓
               MemoryService
                    ↓
Prepared query: intent, scope, reference time, temporal constraints
                    ↓
Eligibility: ownership, project, branch, document type, applicable state
                    ↓
Relevant candidates from existing ordinary and structured retrieval
                    ↓
Existing merge, structured-first priority, and session reranking
                    ↓
One bounded freshness adjustment within each score tier
                    ↓
Stable final ranking, evidence, and score diagnostics
```

Eligibility constraints must take effect before ineligible documents consume the candidate budget where supported. An audit must identify any late filtering and test that it does not starve eligible results. Observation documents remain retained evidence; freshness must not reopen automatic retrieval eligibility for raw observations.

## Ranking contract

For eligible candidates with a comparable, finite, nonnegative baseline relevance score:

```text
age_days = max(0, reference_time - evidence_time) / one_day
freshness = 2 ^ (-age_days / half_life_days)
freshness_delta = relevance_score × weight × freshness
final_score = relevance_score + freshness_delta
```

The relative baseline excludes the legacy generic recency bonus. Explicit temporal queries retain their dedicated temporal handling and receive no generic freshness adjustment. MemoryService applies the relative adjustment only after conversational reranking, segment merging, structured/lexical composition, and session-tag reranking. It preserves structured-first priority and does not compare structured and lexical score magnitudes. Do not stack old and new generic bonuses.

With weight 0.25 and half-life 30 days, boosts are 25%, 12.5%, 6.25%, and 3.125% at ages 0, 30, 60, and 90 days. These are the production defaults unless overridden in project settings.

A score of 5 remains at most 6.25, while an old score of 100 remains 100. However, this formula does not itself establish relevance. Investigate weak matches separately; do not introduce a universal raw-score cutoff across different retrieval modes. Calibrate any abstention rule against labeled relevant, irrelevant, and no-answer queries as a separately measured change.

Before using multiplication, verify baseline score ranges for lexical, hybrid, structured-fact, and segmented retrieval. Resolve incompatible scales using existing fusion machinery. Do not silently clamp invalid scores or assume the structured claim score is directly interchangeable with every other score.

## Intent, validity, and document policy

| Query or content | Policy |
|---|---|
| Current/latest state | Enforce applicable scoped supersession, then use stronger freshness among relevant candidates |
| Ordinary lookup | No generic freshness until a labeled evaluation demonstrates a safe benefit |
| Explanation or durable architecture decision | Little or no generic freshness |
| Explicit historical date, range, or order | Existing temporal resolution and evidence; no decay relative to today's date |
| Comparison of old and new | Keep requested versions available with state labels; no blanket current-only filter |
| Temporary failure or operational status | Evaluate a shorter half-life when existing metadata reliably identifies this category |
| Missing or ambiguous category | Use the ordinary policy; do not infer volatility from provider or source age |

Use existing document metadata and intent enums. Introduce additional metadata only if the implementation audit demonstrates that existing fields cannot represent a required distinction. Do not add an LLM classification dependency.

Supersession remains scoped by ownership, project, entity, and applicable branch/context. A timestamp alone never creates a supersession relationship. Conflicts remain visible. Historical queries must not apply today's current-only eligibility blindly; where historical applicability cannot be reconstructed, expose that limitation rather than inventing a valid-at date.

## Timestamp contract

1. Prefer the timestamp of the evidence supporting the matched fact or passage when available and trustworthy.
2. Otherwise use the original source capture timestamp as a documented proxy. Keep the existing distinction between recording time and event time.
3. Do not select an unrelated chunk's timestamp simply because it appears first. Mixed-age documents need passage-level attribution or an explicitly diagnosed source-level fallback.
4. Enrichment, reindexing, access, replay, or copying an old result into a new transcript must not refresh the original claim's evidence age. Preserve source provenance; repeated quotations are not independent new evidence.
5. Missing or invalid evidence dates receive no generic boost. Future dates clamp to zero age and are diagnosed; they never exceed the configured bound. Historical hard-range eligibility retains its stricter existing behavior.
6. Resolve reference time once per request. Tests and offline evaluation supply a fixed reference time. All segments and structured retrieval use that same value.

Ranking changes alone require no reindex. If existing records lack necessary provenance, plan a separate idempotent repair through `MemoryService::upsert()` and staged publication, affecting only changed documents. Never rewrite original dates to migration time or silently repair the entire corpus during search.

## Configuration and caching

The project server uses `[freshness]` in `.membrick/server.toml`, deserialized into `FreshnessPolicy` and copied into `PipelineOptions`. Supported keys are `mode`, `half_life_days`, and `weight`. Per-intent weights are currently fixed in code; configurable intent overrides and hot reload are not implemented.

Finite half-life and weight validation is implemented at server startup. Weight zero disables the contribution. Invalid settings fail startup; last-good runtime retention, hot reload, and policy diagnostics are not implemented because the current server configuration is read once at startup.

The current on-disk query cache stores index snapshots, not final search rankings. Freshness is recomputed during each query, so the policy and live reference date do not belong in the index-cache key. A ranking policy change does not trigger index reconstruction. If a final-result cache is added later, it must include the effective policy and reference-time semantics or cache candidates and recompute freshness.

## Implementation phases

### 1. Capture a reproducible baseline

- Trace generic freshness, explicit temporal scores, structured-fact scores, segment reranking, and final `MemoryService` ordering.
- Record baseline results, candidate coverage, timestamps, intent, score components, index generation, settings, and fixed reference time.
- Reuse recorded MCP searches and `search_feedback` document judgments. Include stale/current pairs and no-answer cases; manually review ambiguous applicability labels.
- Keep evaluation data and reports decoupled from serving. Evaluate copies or fixtures without mutating the live memory store.

Exit point: each observed bad result is classified as an eligibility, candidate recall, relevance, timestamp, state, or final ranking issue. Document the exact scoring insertion point and any incompatible score scales.

### 2. Implement policy and temporal calculation

- Extend existing configuration and temporal helpers; preserve a legacy comparison mode.
- Resolve intent policy and reference time once through the existing prepared-query path.
- Implement timestamp selection and bounded relative scoring with deterministic tie handling.
- Extend existing score diagnostics with baseline, delta, evidence time/source, effective half-life/weight, and skip reason where needed.

Exit point: deterministic unit cases below pass, and score breakdowns reconcile with totals at the documented stage. Implemented: one request-scoped relative adjustment in MemoryService after final candidate assembly; direct index queries retain legacy behavior and do not apply relative freshness by themselves.

### 3. Integrate all retrieval paths

- Apply the adjustment once through the shared production flow owned by `MemoryService`.
- Cover structured-fact retrieval, single-index search, segmented merge, and adaptive routing.
- Ensure candidate collection does not discard a relevant recent candidate before the final adjustment; measure candidate recall and latency before widening budgets.
- Preserve eligibility and supersession; resolve cache and configuration reload behavior.
- Keep MCP responses concise. Store detailed diagnostics in existing search records/evaluation output rather than adding verbose text to every hook injection.

Exit point: transport-independent results for identical requests and snapshots, no duplicate freshness contribution, no cross-project leakage, and no new writer work during search. Implemented so far: the direct MemoryService path handles single and segmented candidates, preserves structured-first ordering, and expands candidate depth for freshness-eligible intents. Transport and concurrency equivalence still need tests.

### 4. Evaluate changes separately

Compare legacy, generic freshness disabled, relative freshness, and relative freshness with intent policy. Measure any relevance threshold or timestamp repair as a separate variant. Tune parameters on development queries, then freeze them before held-out evaluation. A first 50-question development slice showed that even a low nonzero weight changed 32 result lists and reduced MRR; weight zero exactly matched the disabled baseline. The earlier all-intent relative policy also reduced full-run MRR from 0.888530 to 0.823424. This evidence motivated restricting freshness to `CurrentState`; it does not establish that the restricted policy is safe or beneficial.

Reuse `recency_benchmark`, `project_memory_recency_benchmark`, search-feedback evaluation, and the existing 500-question LongMemEval harness. Extend existing Rust harnesses rather than adding a separate Cargo project. Run single-index and production segmented configurations with the same documents, filters, reference times, and structured-fact setting. Do not claim identical results when candidate routing differs.

Exit point: the validation report below passes its release gates. Production defaults remain unchanged until this evidence exists.

### 5. Roll out and document

- Publish the selected settings, measured tradeoffs, and known limitations.
- Update temporal/search docs, configuration documentation, and release notes.
- Use the benchmark update script for public figures only after the complete report is reviewed.
- Retain configuration rollback to legacy ranking without index rebuild or data loss.

## Validation matrix

| Level | Case | Required result |
|---|---|---|
| Unit | Ages 0, H, 2H; weight zero | Exact expected boost within floating-point tolerance; zero contribution when disabled |
| Unit | Increasing age with fixed relevance | Monotonically decreasing bounded contribution |
| Unit | Missing, malformed, future date | Neutral missing/invalid date; future date bounded and diagnosed |
| Unit | Invalid half-life/weight, NaN, infinity | Configuration rejected; no nonfinite result scores |
| Unit | Equal scores and timestamps | Stable deterministic ordering |
| Unit | Explicit date/range versus ordinary query | Generic freshness only on intended policy paths |
| Unit | Durable explanation versus current-state intent | Correct independent policy selection |
| Unit | Mixed-age chunks and copied evidence | Matched evidence retains its original age; no first-chunk accident or quotation refresh |
| Service | Relevant old document versus unrelated recent document | Recent unrelated content does not become the answer solely through freshness |
| Service | Equally relevant old/current pair | Current applicable answer ranks first in the controlled fixture |
| Service | New unsupported contradiction | No automatic supersession or claim of verified correctness |
| Service | Branch/project/user mismatch | No cross-scope supersession or leakage |
| Service | Historical comparison and as-of query | Requested historical evidence remains accessible and correctly labeled |
| Service | One superseded fact in a multi-fact source | Unaffected facts remain available; superseded text is not reintroduced as current |
| Service | Observation capture and enrichment/replay | Observations stay outside automatic retrieval; updates do not refresh old evidence age |
| Integration | Structured retrieval on/off and both index layouts | Each result gets one policy application; explicit temporal scoring remains correct |
| Integration | Relevant candidate outside initial segment shortlist | Coverage loss is detected and measured, not hidden by reranking top-k only |
| Integration | Cache hit across time/policy changes | Correct recomputation or expiration; no stale policy rankings |
| Concurrency | Searches during config reload and upsert publication | Each request sees a coherent policy and index snapshot; no partial state or panic |
| Transport | HTTP, Unix IPC, filesystem queue, direct service | Same result IDs/order for identical query context and snapshot |
| Recovery | Restart or rollback | Selected valid configuration restored; legacy mode usable without rebuilding |

For ranking bug fixes, first demonstrate the failing fixture on the baseline and then its passing result with the change. Freeze the clock and avoid sleeps in unit tests.

## Evaluation report and release gates

Report baseline and each variant by query ID, with old/new ranks, retrieved IDs, judgment labels, timestamp provenance, applicable state, and score components. Separate candidate misses from ordering changes. Include:

- Current-answer@1: fraction of answerable current-state queries whose first result supports the applicable answer.
- Superseded-result@5: fraction of current-state queries exposing a superseded answer as current in their first five results.
- Judged precision@5 and NDCG@5, with judgment coverage and unjudged counts. Unreviewed documents are not automatically irrelevant.
- Recall@5/10/20 and MRR on the existing benchmark; per-intent slices and every changed query.
- No-answer false-positive rate and abstention rate on labeled no-answer cases.
- p50/p95/p99 latency, throughput, candidate counts, and cache behavior under matched workloads.

Evaluation gates for future freshness changes:

1. All deterministic applicability, isolation, timestamp, cache, and concurrency cases pass.
2. Held-out current-answer@1 improves over legacy and superseded-result@5 does not increase; report denominators and paired uncertainty rather than claiming significance from a tiny set.
3. No aggregate Recall@5/10/20, MRR, or NDCG regression on the fixed 500-question run without an explicit reviewed tradeoff. Every lost gold hit is explained. Aggregate equality alone does not satisfy the freshness gate.
4. Historical, explanation, and no-answer slices show no unexplained quality regression.
5. Proposed performance budget: no more than 5% p95 latency increase or throughput decrease on repeated matched runs. Report run variation; investigate violations before rollout.
6. No new corpus-wide scan, synchronous reindex, or mutation on the query path. Rollback and restart checks pass.

The deterministic formula, intent, configuration, and MemoryService-path tests are implemented. Full 500-question comparisons reject the earlier all-intent policy. The CurrentState-only policy is now the default by explicit product choice: matched repeats show Any-hit@3 improving from 94.6% to 94.8%, while MRR decreases by 0.000433 and NDCG@10 by 0.000985. The user has accepted that small aggregate ranking tradeoff in favor of fresher current-state results. Freshness-specific labeled judgments, candidate coverage analysis, segmented/adaptive comparison, and concurrency/config-change evaluation remain open and should guide subsequent tuning. The 500-question retrieval benchmark is not a substitute for the current-answer and superseded-result judgments defined above.

Two matched repeated runs used the same dataset, heuristic NER, structured-fact retrieval, single-index mode, and cutoffs `k=1,3`. Both repeats produced identical ranking metrics:

| Metric | Legacy | CurrentState-only relative | Change |
|---|---:|---:|---:|
| Fractional Recall@1 | 52.820% | 52.720% | −0.100 pp |
| Any-hit@1 | 82.400% | 82.200% | −0.200 pp |
| Fractional Recall@3 | 84.380% | 84.297% | −0.083 pp |
| Any-hit@3 | 94.600% | 94.800% | +0.200 pp |
| MRR | 0.887652 | 0.887218 | −0.000433 |
| NDCG@10 | 0.879071 | 0.878085 | −0.000985 |

Across these two repeats, mean search time averaged 3.381 ms for legacy and 3.360 ms for CurrentState-only relative freshness. p95 averaged 5.701 ms and 5.626 ms, respectively. These latency results are diagnostic local measurements, not a throughput evaluation.

The detailed per-question reports for these repeats are retained locally. The committed plan records the aggregate results and methodology; this experiment does not replace the published benchmark headline.

### Existing 500-question single-index comparison

These runs use the same LongMemEval-S dataset, heuristic NER, structured-fact retrieval, single-index mode, and cutoffs `k=1,3,5,10,20`. They are offline experiments and do not update public benchmark figures.

| Metric | Legacy | Disabled | Relative, weight 0.25, CurrentState only | Relative, weight 0.25, earlier all-intent policy |
|---|---:|---:|---:|---:|
| Fractional Recall@1 | 52.820% | 52.820% | 52.720% | 44.930% |
| Fractional Recall@5 | 89.783% | 89.783% | 89.817% | 89.083% |
| Fractional Recall@10 | 95.197% | 95.197% | 95.130% | 95.047% |
| Fractional Recall@20 | 98.337% | 98.337% | 98.337% | 98.053% |
| Any-hit@1 | 82.400% | 82.400% | 82.200% | 70.200% |
| Any-hit@5 | 96.600% | 96.600% | 96.800% | 96.600% |
| Any-hit@10 | 98.800% | 98.800% | 98.800% | 98.600% |
| Any-hit@20 | 100.000% | 100.000% | 100.000% | 99.800% |
| MRR | 0.888530 | 0.888524 | 0.888091 | 0.823424 |
| NDCG@10 | 0.879071 | 0.879061 | 0.878085 | 0.836240 |
| Mean search time | 3.703 ms | 3.656 ms | 3.782 ms | 3.548 ms |
| p50 search time | 3.474 ms | 3.434 ms | 3.442 ms | 3.344 ms |
| p95 search time | 6.199 ms | 6.126 ms | 6.630 ms | 5.873 ms |
| p99 search time | 7.318 ms | 7.191 ms | 8.340 ms | 6.930 ms |

The earlier all-intent relative variant fails the quality gate despite slightly lower measured search latency. A smaller weight did not materially change that result, and stable tie handling did not explain the regression. The `CurrentState`-only policy is much closer to legacy: 196/500 top-20 lists changed, but only five questions changed MRR (two improved, three declined). Its aggregate MRR decreased by 0.000439, NDCG@10 by 0.000985, and mean search time increased by 0.079 ms. The observed p95 increased by 6.95%, over the proposed 5% budget; this is a single run and needs repeated matched measurements before attributing the difference to freshness. The benchmark-category slices show a mixed result: temporal-reasoning MRR increased from 0.852682 to 0.858321, while multi-session MRR decreased from 0.894361 to 0.887093. Dataset question categories are not the same thing as runtime query intents, so these slices do not establish the intended CurrentState effect.

The legacy-versus-disabled comparison isolates removal of the old generic boost: top-20 lists changed for 179/500 questions, but only one MRR changed (−0.0029), while aggregate recall and NDCG stayed effectively equal. The disabled-versus-relative comparison isolates the `CurrentState` policy: 32/500 top-20 lists changed and four MRRs moved, two up and two down, for a net MRR delta of −0.000433. Those four queries were `6d550036` (+0.033), `46a3abf7` (−0.500), `27016adc` (−0.500), and `gpt4_2f56ae70` (+0.750). The fifth MRR change in the legacy-versus-relative comparison was `0862e8bf_abs` (−0.003), due to removal of the old generic boost. Review these queries' full text and document ranks before any rollout decision.

The current 500-question artifacts were written to `/tmp/membrick-freshness-final-single-legacy.json`, `/tmp/membrick-freshness-disabled-500.json`, and `/tmp/membrick-freshness-currentstate-500.json`. These local diagnostic reports are not public benchmark updates. The disabled and relative runs used weight 0.25, heuristic NER, structured-fact retrieval, single-index mode, and `k=1,3,5,10,20`.

## Practical references

[Elasticsearch function score](https://www.elastic.co/docs/reference/query-languages/query-dsl/query-dsl-function-score-query) describes decay functions and score combination. [Vespa nativeRank](https://docs.vespa.ai/en/ranking/nativerank.html) describes combining relevance with freshness features. They support the general approach; Membrick's weights, integration point, and rollout gates require our own evaluation.
