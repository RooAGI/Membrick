# Case Study: Retrieval-First Know-How Distillation with Membrick

**Status:** Workflow exploration with small, audited topic pilots  
**Corpus:** NVIDIA Developer Forum exports indexed by Membrick  
**Topics explored:** DGX Spark vLLM inference, thermals/cooling, training/fine-tuning, other workloads, and system updates

## Summary

The goal is to create useful, evidence-backed know-how without sending every forum post through an LLM. The proposed workflow uses Membrick to find candidate source documents, retrieves their indexed `SourceDocument` records, and distills only selected documents. A person can author the know-how; a model can be added for the selected sources when automation is useful.

The pilot showed that `review_search(search_id)` can return a complete source document when it is within the current size limit. It also showed why one broad search is insufficient: the results included relevant vLLM threads and unrelated inference/performance documents. The first vLLM 26.04 example is enough to demonstrate source-document retrieval, but it is not enough to establish a reliable compatibility recommendation. A targeted follow-up found a second thread with a different result on another R580 driver version.

## What counts as know-how

For this case study, know-how is practical, reusable information about how to perform a task or handle a problem. A record should:

- state a procedure, fix, compatibility result, configuration, performance result, limitation, fact, or tip;
- be supported by specific source posts;
- preserve hardware and software scope, uncertainty, and conflicts;
- link back to the source document and post numbers.

A question or untested suggestion is not a confirmed fix. A single successful report can be retained as a single-report result when its scope is clear.

## Workflow explored

1. **Define a topic as a search query.** Start with the topic and its useful synonyms, versions, symptoms, and task terms.
2. **Search Membrick.** Treat ranked hits as candidate documents, not an exhaustive topic inventory. Collect and deduplicate their `doc_id` values.
3. **Retrieve each `SourceDocument`.** Call `review_search` with the search ID. Check the source hash and `content_truncated` flag before using the content.
4. **Triage candidates.** Mark documents as relevant, partially relevant, or irrelevant. Search again with narrower or complementary queries where coverage is uncertain.
5. **Draft know-how from selected sources.** Extract only claims supported by the source posts. Record scope, status, caveats, and post-level evidence.
6. **Check for duplicates and counterevidence.** Search Membrick for related claims and different versions/setups before consolidating records.
7. **Store know-how separately from the original source.** Preserve the source document unchanged and keep evidence links on the new knowledge record.

This is not a BFS traversal by itself. Each Membrick search returns ranked results. A separate, bounded expansion process could search terms or links found in retrieved documents, but it should track visited document IDs and stop when new relevant sources stop appearing.

## Build plan: a bounded search-and-triage stage

Add triage as a discovery stage before the existing normalize/segment/extract pipeline. It should produce a reviewed list of thread IDs for extraction and a ledger showing how those IDs were found and why they were selected. Keep the first version bounded and reviewable; do not crawl every link or send every result to the extraction model.

### First version

1. **Create a topic plan.** Record the topic definition, inclusion/exclusion rules, and a small set of subtopics. For `inference-vllm`, use installation, startup/memory, model/driver compatibility, and multi-node serving.
2. **Run seed searches through Membrick.** Save each exact query, Membrick `search_id`, ranked `doc_id`s, ranks, and source hashes. Deduplicate candidates by `doc_id` across searches.
3. **Review candidate threads.** Hydrate candidates with `review_search`, record truncation, and assign one triage decision: `selected`, `review`, `exclude`, or `follow_up_search`. Add a short reason and the relevant subtopic. Selected means “worth extracting,” not “claim is true.”
4. **Expand only where review finds a gap.** Generate a small number of focused searches from missing versions, symptoms, or setups seen in selected threads. Search for both successful outcomes and failures. Keep a visited-query set and candidate-ID set, and cap both searches and rounds for the pilot.
5. **Hand selected IDs to distillation.** Export the chosen local thread IDs as a file, then feed that file to the existing `normalize`/`segment`/`extract` steps. Preserve the ledger alongside `work/items/` so later merges can trace provenance.

Suggested files are `work/triage/topic.json`, `work/triage/searches.jsonl`, `work/triage/candidates.jsonl`, and `work/triage/selected-threads.txt`. Candidate records should include the local thread ID, Membrick `doc_id`, source hash, queries/ranks, decision, reason, subtopic, and whether the complete source was reviewed. A small validation command can ensure selected IDs exist in the raw export and that reviewed source hashes match the recorded revision.

The initial pilot can be run by an agent or operator using the Membrick MCP search/review tools and writing this ledger; the current Python CLI does not itself call Membrick. Once the ledger format and decisions work on a sample, add a CLI import/validation step. Add direct Membrick API integration only if the MCP surface is exposed to the distill process.

### How to judge whether triage is working

Use **F1 on an audited candidate sample** as the primary triage metric. It balances keeping useful threads (`recall`) against excluding irrelevant threads (`precision`):

```text
precision = TP / (TP + FP)   # selected threads that are judged useful
recall    = TP / (TP + FN)   # useful threads retained from the candidate pool
F1        = 2 * precision * recall / (precision + recall)
```

Here, “useful” means the complete source supports at least one potentially reusable know-how claim with traceable post evidence—not merely that its title matches the topic. Build the audit set by sampling selected and excluded candidates across subtopics, then have a person judge them from the complete source. Keep the sampled IDs and judgments so the score can be reproduced. If the sample oversamples a small subtopic or decision class, weight the counts back to the candidate pool proportions.

When topics are sampled equally, report the **macro-average of per-topic F1** so a large topic does not hide a weak one. Report micro-F1 as a secondary pooled number only when the sample weights reflect the candidate-pool sizes.

Also report **validated know-how yield per extraction call** and the **selection rate** (`selected / unique retrieved candidates`) to show whether a good F1 is saving model work and producing useful records. Set the acceptable F1 and cost targets after the first labeled pilot rather than choosing an arbitrary threshold in advance.

F1 measures triage only within the documents Membrick retrieved. It cannot detect relevant documents absent from the candidate pool. Track search coverage separately by auditing known relevant examples and recording how many new useful candidates each follow-up query adds. Stop expanding when the yield of new useful candidates is low and no known subtopic gap remains; this is a practical stopping rule, not proof of exhaustiveness.

### Small random-topic pilot

To exercise the calculation, I sampled three slugs from the 15 configured topics with a seeded Fisher–Yates shuffle (`20261116`): `thermals-cooling`, `training-finetuning`, and `other-workloads`. I ran one broad Membrick query per topic; for `other-workloads`, I added speech/ASR, computer-vision, and Isaac/robotics probes because the broad query mixed LLM serving, audio hardware, and non-LLM workloads. From the union of those forum `raw/` result neighborhoods, I built topic-specific selected/excluded pools, then drew two candidates from each decision class per topic with seed `123456789`. Source review supplied the audit label. “Relevant” means the thread can support reusable know-how within that topic's current definition.

| Topic | Selected sample → audit label | Excluded sample → audit label | TP / FP / FN / TN | F1 |
| --- | --- | --- | --- | ---: |
| `thermals-cooling` | `377069` relevant; `367883` relevant | `387379` not relevant; `380975` not relevant | 2 / 0 / 0 / 2 | 1.00 |
| `training-finetuning` | `378311` relevant; `346967` not relevant | `380975` not relevant; `383193` not relevant | 1 / 1 / 0 / 2 | 0.67 |
| `other-workloads` | `364191` relevant; `385294` relevant | `383193` not relevant; `373819` not relevant | 2 / 0 / 0 / 2 | 1.00 |

The strict topic-level **macro-F1 is 0.89**: `(1.00 + 0.67 + 1.00) / 3`. The pooled sample has TP=5, FP=1, FN=0, TN=6, giving micro-F1 `0.91` (precision `0.83`, recall `1.00`). The one false positive was `346967`: its title matches fine-tuning, but its only answer is generic advice from someone who says they do not own a Spark, so it does not support a reliable technical know-how item under this topic definition.

This is a demonstration, not a performance estimate: it is only 12 audited candidates, the excluded examples include clear cross-topic negatives, and no false negative appeared in this small draw. It does not measure missed documents outside the retrieved set. The sample also shows why topic boundaries matter: if `training-finetuning` is expanded to include untested hardware-buying opinions, the audit label for `346967` changes and its F1 rises. Repeat the audit with a larger, independently sampled pool before setting an acceptance threshold.

#### Run ledger

The machine-readable record for this run is [`triage-pilot-2026-11-16.json`](triage-pilot-2026-11-16.json).
[`triage-pilot.py`](triage-pilot.py) recalculates the recorded F1 metrics and checks that audited candidates belong to their recorded pools. Pass `--source-root <forum-export-directory>` to also verify the source-file hashes. It validates this saved run; Membrick searches still need to be run through the Membrick tools and recorded in the ledger.

The topic universe was the 15 slugs in `distill/config.py`. The recorded selection is `thermals-cooling`, `training-finetuning`, and `other-workloads`, with seed `20261116`. The selection note says seeded Fisher–Yates, but the PRNG implementation and full selection script were not saved, so that seed alone is not sufficient to reproduce the draw exactly.

Membrick searches used these queries and search IDs (the first three broad queries were initially run with `top_k=8`; a later pass used `top_k=15`):

| Purpose | Query | Search IDs |
| --- | --- | --- |
| Thermals/cooling | `DGX Spark thermal temperature fan cooling throttling` | `mcp:c41c38eb9ff198760b4888f8435cf9fd8cf2da75edc0f2581beff4c0a96db676` (`top_k=8`); `mcp:be6786f6222280ec4073a9a2b5479eda071a669d63652a2a25fbb3caf9cb2004` (`top_k=15`) |
| Training/fine-tuning | `DGX Spark training fine tuning LoRA` | `mcp:6dd9f7adb504233784ceccb35401161359c2169fe51b9c52c5bfa64d8cb0f785` (`top_k=8`); `mcp:231001276da61c7d3c920a2d81ed2c2a38960116e40340e08f5379291c767ed7` (`top_k=15`) |
| Other workloads | `DGX Spark audio vision robotics scientific workload` | `mcp:7ab4d5b1f91623cef0687480eea20223d6237996210e4b40e07b09f672115e18` (`top_k=8`); `mcp:8ce9b4e89052669f42e9ecc685843fb828ec560376e14cf38c68c6c8dc3d8d8c` (`top_k=15`) |
| ASR probe | `DGX Spark speech recognition ASR transcription audio model Parakeet` | `mcp:7db322600864f4b028db142ff2b069af75406d0523977f563eb8807deaccc37f` |
| Computer-vision probe | `DGX Spark computer vision image classification detection pipeline` | `mcp:100c76f2e9ad7731d6a1b647e62ffadde9aed394a3dfe8349f72d58071916e09` |
| Robotics probe | `DGX Spark robotics Isaac ROS simulation` | `mcp:d21e8dad4ee68dc170e94eedf7497f7fe827fd96348815f0c9c11ab0df1be72` |

The topic-specific candidate pools used for the audit draw are recorded below. These are manually assembled result neighborhoods from the searches above, not exhaustive corpus inventories. The two audit samples per decision class were drawn with seed `123456789` using xorshift32; the sample IDs and class assignments are listed explicitly so the audit can be checked even though the original draw script was not saved.

| Topic | Selected pool | Excluded pool |
| --- | --- | --- |
| `thermals-cooling` | `377069, 367883, 383193, 377044, 373996, 383988, 381029, 361960, 382065, 373836` | `361703, 387379, 380975, 373819` |
| `training-finetuning` | `371355, 376822, 361703, 363268, 381381, 346967, 387379, 378311, 366223, 351329` | `383193, 380975, 373819` |
| `other-workloads` | `364191, 356353, 385294, 347965, 365780, 352698` | `383193, 373819, 361703, 387379` |

Source review decisions and Membrick source hashes for the audited documents. These are Membrick-provided provenance hashes, not local-file SHA-256 values:

| Topic | ID | Triage decision | Audit label | Membrick source hash |
| --- | ---: | --- | --- | --- |
| Thermals | `377069` | selected | relevant | `7a83431dc69701b519978d70450755b0efa43b454ca2271f257b485ad688f78e` |
| Thermals | `367883` | selected | relevant | `97c8b91b4c21bbacafcd37cc6b9cf5954da37abd486606331a552310845f076a` |
| Thermals | `387379` | excluded | not relevant (training) | `43b89482e4e54e8f48e0aeed933ab20c39aa3a8ae94de7aa8a5dcb9f6d0eb8ad` |
| Thermals | `380975` | excluded | not relevant (multimodal llama.cpp) | `94cccf3fe66d38438688020edc5310267300c89f79b420292a5f89d054ba9743` |
| Training | `378311` | selected | relevant | `7aa2a4612d6f15c2d24f70a0c8b61f84bd10e4c466b0dff6f8408207a2c5e42f` |
| Training | `346967` | selected | not relevant (generic advice; respondent says they do not own a Spark) | `a40246bb42ef471e57aed1c4bb16a960e500b8fbf28b0798720f900fb92db393` |
| Training | `380975` | excluded | not relevant (multimodal llama.cpp) | `94cccf3fe66d38438688020edc5310267300c89f79b420292a5f89d054ba9743` |
| Training | `383193` | excluded | not relevant (cooling) | `d784f251a4df48acb5adce412ab9c028e04686d851a5fb9e6e4299c6a064e6ad` |
| Other workloads | `364191` | selected | relevant (ASR) | `2c607d26acc1ef742e0d4f387581c7278fd18f2f43fd16603ee69ec76008b0cc` |
| Other workloads | `385294` | selected | relevant (VibeVoice ASR) | `6d2e6bd413b0e795b2e8e900b8a0784ef1e4a4cbf31638c65aa7a8377dd7e886` |
| Other workloads | `383193` | excluded | not relevant (cooling) | `d784f251a4df48acb5adce412ab9c028e04686d851a5fb9e6e4299c6a064e6ad` |
| Other workloads | `373819` | excluded | not relevant (cross-node LLM serving) | `197748eab41785fc31eff2ce70ec81f12aec1a786541736ae7d82e54ab17cffc` |

The Membrick source review reported truncated content for `378311`, `364191`, and `373819`; their labels were checked against the available local source content. `385294` was not truncated. This ledger captures the executed searches, candidate neighborhoods, sample, labels, and metric arithmetic, while making clear that the random selection scripts were not retained. A future run should save the exact scripts and the full Membrick result export alongside this ledger.

### Additional topic pilot: `system-updates`

This second run tested the configured `system-updates` topic. The inclusion boundary was DGX Spark DGX OS/OTA, BIOS, firmware, NVIDIA driver, and kernel updates and their regressions. A source counted as relevant when it could support a reusable, evidence-backed fact, diagnostic procedure, outcome, limitation, or carefully scoped report. Unresolved reports counted when their uncertainty stayed explicit. Recovery-tool requests and general hardware/workload problems without an update trigger were excluded.

Two Membrick searches supplied complementary candidate neighborhoods:

| Search | Query | Search ID | Results |
| --- | --- | --- | ---: |
| Broad | `DGX Spark DGX OS update firmware BIOS driver kernel upgrade regression recovery` | `mcp:fa3f1bf612e6dff2935e81c60080ff41edc794341f1d2475004f98b11107cbf0` | 12 |
| Follow-up | `DGX Spark apt upgrade NVIDIA driver firmware OTA version mismatch GPU no devices rollback` | `mcp:c2d18dabc49291dd781b9fd40f06cb567052958aeb0097ed51a7306a96614481` | 12 |

The follow-up added eight new document IDs and repeated four IDs from the first query (`378200`, `369711`, `387898`, `348453`). I deduplicated on `doc_id`, leaving 20 unique candidates. The exact ranked results and Membrick source hashes are in [`triage-pilot-2026-11-16-system-updates.json`](triage-pilot-2026-11-16-system-updates.json). The pool contained 15 selected and 5 excluded candidates. Three from each class were sampled without replacement from sorted IDs using `random.Random(20261116).sample`.

| Thread | Candidate rank | Triage | Source audit |
| --- | --- | --- | --- |
| [361630](https://forums.developer.nvidia.com/t/361630) | Broad #12 | selected | Relevant: posts #5–6 report display signal loss after updates; useful as a scoped regression report, with no fix established in that thread. |
| [387898](https://forums.developer.nvidia.com/t/387898) | Broad #6; follow-up #12 | selected | Relevant: detailed, unresolved USB-C PD firmware update report with version checks and diagnostic attempts; not a confirmed fix. Membrick content was truncated, so I checked the local forum export. |
| [375537](https://forums.developer.nvidia.com/t/375537) | Follow-up #7 | selected | Relevant: isolates a fwupd/libfwupd version mismatch after OTA and records a resolution. Membrick content was truncated, so I checked the local forum export. |
| [381820](https://forums.developer.nvidia.com/t/381820) | Broad #9 | excluded | Not relevant to this boundary: asks for recovery tools, without a concrete update regression. |
| [371102](https://forums.developer.nvidia.com/t/371102) | Broad #10 | excluded | Not relevant to this boundary: proposes a recovery design rather than documenting an update event or regression. Membrick content was truncated, so I checked the local forum export. |
| [388605](https://forums.developer.nvidia.com/t/388605) | Follow-up #9 | excluded | Not relevant to this boundary: correlates a freeze with Electron GPU rendering on a matching OS/driver stack, without an update trigger. |

Running the saved calculation script against the new ledger produced TP=3, FP=0, FN=0, TN=3. Precision and recall were both 1.00, so F1 was **1.00**. The command also checked the six local source SHA-256 values against the forum exports. Membrick's source hashes are stored separately because they do not equal the local files' SHA-256 values.

This score applies only to the six audited candidates. It is not a quality estimate, and it says nothing about relevant documents absent from the two search result lists. The perfect sample score is especially fragile at this size; no false negative appeared in the sample.

## Define topics from the forum as it is explored

Do not browse every thread to invent a taxonomy. Start from Membrick's indexed result neighborhoods and define a provisional topic map from repeated search patterns. Read source threads only after the index suggests which clusters and boundaries need validation.

### What Membrick's index tells us

Membrick search is a ranked retrieval view over indexed documents. A result can give us its `doc_id`, a relevant snippet, rank/score, matched terms/entities, source hash, and (through `review_search`) the indexed `SourceDocument`. This is enough to suggest clusters and formulate the next query without opening full forum threads.

It is not a corpus catalog: a top-k result list does not tell us the total number of documents in a topic, all terms in the index, or whether a missing result means a topic is absent. Ranks and scores are retrieval signals, not prevalence counts. Search can also return Membrick session/outcome documents, so first filter candidates to the intended forum sources (for this corpus, `raw/<thread-id>/<thread-id>.md`) and do not treat project-session records as forum evidence.

### How to start the index search

1. **Pick one broad domain phrase.** Use a distinctive system or tool plus the user task. For example, start with `DGX Spark vLLM`. Avoid packing setup, performance, compatibility, and every possible symptom into one query.
2. **Save the first result neighborhood.** Record the exact query and search ID, then deduplicate the forum `doc_id`s. For each hit, retain its title/snippet, rank, matched terms/entities, and source hash. Do not open full threads yet.
3. **Name visible clusters from repeated patterns.** In the `DGX Spark vLLM` results, titles and terms surfaced two-Spark deployments, Ray/tensor parallelism, ConnectX networking, model launch failures, and compatibility. These suggest candidate tasks; a single hit does not establish a subtopic.
4. **Probe the boundaries with task queries.** Search separately for `DGX Spark vLLM install container build`, `DGX Spark vLLM driver CUDA compatibility`, and `DGX Spark vLLM Ray tensor parallel multi-node NCCL`. Add a success/failure or symptom probe where the initial results point to one. Keep the query set attached to its proposed subtopic.
5. **Compare result overlap and non-overlap.** Repeated documents and terms across a query family support a cluster. Results that appear only in one broad query may be noise or a separate topic. Use a contrast query—such as the same hardware plus another runtime—to see whether a symptom belongs to vLLM or cuts across engines.
6. **Write the topic boundary as a search contract.** State what user task is in scope, what adjacent tasks are out of scope, the queries that retrieve it, and a few representative `doc_id`s. Only then review a small set of representative source documents to validate the boundary and the actual know-how.

This index-first pass creates candidate topics without browsing every thread. It does not remove source review: source review is a later validation step for selected examples and extracted claims.

I probed the index with `DGX Spark vLLM`, `DGX Spark model serving setup container`, `DGX Spark driver CUDA compatibility updates`, `DGX Spark multi-node networking Ray NCCL`, and `DGX Spark model memory OOM loading`. The result titles suggest:

| Index signal | Candidate topic/subtopic boundary |
| --- | --- |
| `Ray`, `NCCL`, `node`, `network` recur together in multi-node results (for example, threads 373387, 355126, 377435, and 365664) | A coherent multi-node serving/clustering subtopic |
| Driver/CUDA query returns both general driver/update threads (364688, 363299, 368114) and vLLM compatibility threads (368935, 379168) | Driver updates are a broader system topic; runtime compatibility is a vLLM subtopic with driver/CUDA as facets |
| Memory/OOM query returns Hugging Face loading (361990, 373341), TensorRT, SGLang, and vLLM-related results | “Memory/OOM” is too broad as one engine-specific subtopic; use it as a symptom facet and split by workload/runtime when the index supports distinct clusters |
| “Serving setup/container” returns setup, networking, and model-serving material | The query is too broad to define a topic; refine it into task-oriented probes |

**Provisional topic contract:** `DGX Spark vLLM` covers installing/building vLLM, serving a model with vLLM, diagnosing vLLM launch/runtime behavior, checking vLLM/model/driver compatibility, and distributing vLLM serving across Spark nodes. It excludes general DGX OS/driver maintenance unless it affects a vLLM setup, other serving engines unless compared with vLLM, and Hugging Face loading/fine-tuning threads that do not use vLLM. Start with the broad seed `DGX Spark vLLM`, then run the four task probes listed above. Treat `Ray`/`NCCL`/multi-node as one candidate cluster, driver/CUDA as a compatibility facet plus a broader system-update neighbor, and memory/OOM as a cross-engine symptom until filtered by runtime.

This is index evidence for **candidate** boundaries, not a measured frequency table. Membrick's current `search` surface returns ranked matches, not a complete corpus listing, cluster assignments, or corpus-wide term counts. So we can define a useful provisional taxonomy from repeated retrieval neighborhoods, but we cannot claim that a cluster is exhaustive or dominant from top-ranked results alone. To improve that later, add an index-side operation that exports aggregate document counts/terms or cluster summaries; keep source-thread review for validation samples and evidence, not for constructing the whole map.

Keep three different things separate:

- **Topic:** the broad body of know-how, such as serving models with vLLM on DGX Spark.
- **Subtopic:** a recurring task or problem with its own useful search plan and know-how, such as installing/building vLLM, diagnosing model startup or memory failures, checking model/driver compatibility, or serving across multiple nodes.
- **Facets:** details that describe the scope of an item across many subtopics, such as driver/CUDA version, model and quantization, container tag, Spark model, node count, or Ray. Record these on each thread/item; do not create a new subtopic for every version or model.

For each sampled thread, first state the user's main goal or question in plain language. Give it one primary subtopic and allow secondary facets or links when it genuinely crosses boundaries. Promote a recurring pattern to a subtopic when it represents a distinct task, has enough relevant threads to search, or needs a different know-how structure. Keep a one-off issue as a candidate label until more evidence appears. Merge labels when they produce the same search set and the same kind of useful answer; split them when their procedures, evidence, or intended answers differ.

Use the forum's own title and opening problem as initial clues, then correct them using the replies and accepted outcome. Search terms should use the language found in those posts, including product names, error strings, versions, and synonyms. In the current export sample, the `.export.json` manifest carries source and post-number metadata but no category/tag taxonomy, so do not assume forum categories are available as labels.

### Provisional map for the vLLM pilot

| Working subtopic | Include | Example thread | Cross-cutting facets |
| --- | --- | --- | --- |
| Install and build | Container choice, host/source builds, dependencies, setup tools | [348801](https://forums.developer.nvidia.com/t/348801) | base image, vLLM version, CUDA toolkit, architecture target |
| Model startup and runtime failures | vLLM tokenizer/model initialization errors, OOM, and launch configuration | [370519](https://forums.developer.nvidia.com/t/370519) | model, quantization, memory flags, container |
| Model/driver compatibility | Whether a model and vLLM release can run with a specific driver/CUDA path | [368935](https://forums.developer.nvidia.com/t/368935), [379168](https://forums.developer.nvidia.com/t/379168) | driver, CUDA, model architecture, container tag |
| Multi-node serving | Ray, tensor/pipeline parallelism, placement and inter-node networking | [374812](https://forums.developer.nvidia.com/t/374812), [365664](https://forums.developer.nvidia.com/t/365664) | node count, NIC/switch, Ray, parallelism setting |

This is a working map, not a final taxonomy. For example, thread [361990](https://forums.developer.nvidia.com/t/361990) mentions a DGX Spark and model-loading memory, but its main task uses Hugging Face `from_pretrained()` for fine-tuning, not vLLM serving. Triage should mark it as related/outside the current vLLM topic and route it to a model-loading or training topic if that topic is in scope. Hardware or the word “model” alone should not pull a thread into vLLM.

Keep a small topic registry with each label's definition, inclusion/exclusion examples, known aliases, parent, and status (`candidate`, `active`, `merged`, or `split`). When labels change, record the mapping so existing items remain traceable. Review the registry after each batch of threads and after each search round; this is how the taxonomy grows from the forum rather than becoming a fixed guess up front.

## Pilot: `inference-vllm`

### Initial candidate search

The broad query was:

> DGX Spark vLLM inference serving setup configuration troubleshooting fixes compatibility performance

Among its top results were:

| Document | Initial assessment |
| --- | --- |
| `raw/368935/368935.md` — vLLM Release 26.04 | Relevant: driver and container compatibility |
| `raw/351559/351559.md` — vLLM inference with two ConnectX-7 NICs | Relevant: multi-node serving and networking |
| `raw/362518/362518.md` — two-Spark tensor-parallel initialization failure | Relevant: multi-node troubleshooting |
| `raw/365664/365664.md` — MiniMax on two Sparks with vLLM | Relevant: model-specific multi-node setup |
| `raw/357249/357249.md` — reproducing FP4 performance with CUTLASS | Off-topic for vLLM serving |
| `raw/374419/374419.md` — Flux.2 Klein with Nunchaku | Off-topic for vLLM; another inference workload |

This result set is useful for discovery, but its relevance mix means the search needs more focused passes. The topic should also be split into subtopics such as installation, memory/OOM, model compatibility, and multi-node serving.

### SourceDocument retrieval

A focused search for NVIDIA vLLM 26.04, CUDA forward compatibility, kernel driver 580.142, and Qwen3-8B returned `raw/368935/368935.md`.

`review_search(search_id)` returned its indexed `SourceDocument` with:

- `content_bytes`: 7,058
- `content_truncated`: `false`
- `source_hash`: `d44da071bdf0ba4810512ea3d7bcd732a383665e6d32d1d9ede362794663b4c5`

The complete forum content was available from Membrick, so this source did not need to be reopened from the local raw export.

### Draft know-how item

**Kind:** Compatibility  
**Title:** NVIDIA vLLM 26.04 container launch with CUDA forward compatibility  
**Statement:** In one reported test, the NVIDIA vLLM 26.04 container ran with CUDA forward compatibility enabled, using CUDA 13.2 driver version 595.58.03 over kernel driver 580.142. Inside the container, `vllm serve Qwen/Qwen3-8B` reached “Application startup complete.”  
**Scope:** NVIDIA Release 26.04; vLLM `0.19.0+6bc3197f`; CUDA compatibility driver 595.58.03; kernel driver 580.142; model `Qwen/Qwen3-8B`. The tester did not specify the hardware or OS.  
**Status:** Works for the reported launch test.  
**Evidence:** [Forum post #5](https://forums.developer.nvidia.com/t/368935/5), source document `raw/368935/368935.md`.  
**Caveats:** The container reported a default SHMEM allocation limit of 64 MB and said it may be insufficient. It printed recommended Docker flags, but the post does not say whether they were applied. The hardware is unstated, so this evidence does not establish the same result on a DGX Spark.

This is deliberately narrow: it records what the test demonstrated, without transferring the thread opener’s GB10 Spark scope to a different poster’s test.

### Follow-up search: a second, different compatibility result

A targeted Membrick search for other reports of vLLM 26.04 compatibility on driver 580.142 returned `raw/379168/379168.md`.

Its `SourceDocument` was 12,279 bytes, so `review_search` returned it with `content_truncated: true` at the 8 KiB limit. The remaining source was checked in the local forum export. Its source hash was `850ae47d9ee501de6ab23eb8bffd6a3c50a8f0554072c17b94ac458062dda766`. This is a concrete example of why the truncation flag must be honored before drawing conclusions from the retrieved text.

The second thread reports a DGX Spark GB10 with kernel driver 580.173.02 and `nvcr.io/nvidia/vllm:26.04-py3`:

- In post #1, the poster says the container reports compatibility mode unavailable after a driver update from 580.159.03 to 580.173.02. The container falls back and can serve requests, but not using the intended forward-compatibility path.
- In post #5, the poster confirms that the older 26.02 image enables CUDA 13.1 forward compatibility on 580.173.02, but it cannot load the poster’s Qwen3.6 model because its vLLM/Transformers stack lacks `qwen3_5` support.
- In post #9, `zneill` confirms the gap described in post #8: vLLM 26.04 needs the CUDA 13.2 / driver 595.58 compatibility level, while the 580.173.02 forward-compatibility path reaches CUDA 13.1. The reply also corrects an earlier mistaken image-tag recommendation. The export does not mark this author as NVIDIA staff, so this should not set a `staff_backed` flag without separate verification.
- In post #10, the poster says the 26.04 image’s native fallback serves the model at approximately 17–20 tokens per second; this is distinct from successful forward compatibility.

Together with post #5 of the first thread, the evidence supports two scoped observations, not a general statement that “vLLM 26.04 works on R580”:

| Source and setup | Observed result |
| --- | --- |
| [Thread 368935, post #5](https://forums.developer.nvidia.com/t/368935/5): vLLM 26.04, kernel driver 580.142, CUDA compatibility driver 595.58.03, Qwen3-8B; tester hardware unstated | Forward compatibility was enabled and the server reached application startup |
| [Thread 379168, posts #1, #5, #8–#10](https://forums.developer.nvidia.com/t/379168): DGX Spark GB10, kernel driver 580.173.02, vLLM 26.04, Qwen3.6-27B-NVFP4 | Intended compatibility mode was unavailable; native fallback reportedly served requests, while CUDA 13.1 forward compatibility from 26.02 could not load that model |

The records should preserve these as version- and model-scoped variants. They do not prove why 580.142 and 580.173.02 produced different observations, and the first test has no stated hardware. That uncertainty belongs in the knowledge record rather than being flattened into a universal recommendation.

## Why one search is not enough

Membrick search ranks likely matches; it does not certify that all relevant documents were found. A broad query can miss alternative wording and versions, while returning adjacent but out-of-scope material.

The follow-up search changed the initial reading: the first source looked like a positive compatibility result, while the second exposed a version/model-specific gap. For this compatibility claim, useful follow-up searches include:

1. **Same behavior:** `vLLM 26.04 CUDA forward compatibility kernel driver 580.142`
2. **Failures or limits:** `vLLM 26.04 driver 580.142 startup failure unsupported CUDA`
3. **Related installation path:** `DGX Spark vLLM CUDA 13 container versus host pip installation`

For the broader vLLM topic, run separate searches for startup memory/OOM, model compatibility, installation, and multi-node serving. Deduplicate document IDs across queries and retain relevance judgments for tuning the search strategy. Then search for corroborating and contradictory reports about the exact claim, version, and setup.

Stop when the subtopic queries yield few new relevant sources and selected claims have been checked against available counterevidence. A search with no counterexample is not proof that none exists.

## Reliability rules for publishing know-how

Treat reliability as a set of checks on each claim, rather than a single confidence score for a whole thread. Membrick finds candidate evidence; the source posts support or fail to support each proposed claim.

1. **Search for coverage.** Split a topic into operational subtopics and run several focused searches: the normal path, common symptoms, exact versions/configurations, and failure or counterexample wording. Deduplicate results by `doc_id`, record the queries and relevance judgments, and track how many new relevant documents each pass adds. Search ranking and an empty result set do not prove completeness.
2. **Read the complete evidence.** Retrieve the `SourceDocument`, verify its `source_hash`, and reject a truncated document as complete evidence. Fetch remaining pages from the same source revision or use the original export until the whole thread is available.
3. **Write atomic claims.** Keep one outcome or instruction per record. Attach each command, version, error, and result to the post that states it. Distinguish what the poster tried from what they recommend, and mark unknown hardware or software scope as unknown rather than borrowing it from the thread opener.
4. **Label evidence strength.** A question or suggestion is unverified; one described test is a single report; matching independent reports support corroboration; a directly contradictory report in the same scope is a conflict. Different driver, container, model, or hardware versions are scoped variants unless the evidence shows they contradict each other. Forum evidence records reported experience, not universal compatibility.
5. **Search the exact claim both ways.** Look for the same setup and outcome, then search for failures, workarounds, and newer-version changes. Keep conflicting or superseded records linked and visible; only mark a record superseded when a newer source explicitly replaces it for the same scope.
6. **Validate before publishing.** Check in code that each evidence link resolves to the recorded document hash and post, required scope fields are present or explicitly unknown, and status matches the evidence label. Have a person review contested, high-impact, or operationally risky instructions. A model can draft from selected complete threads, but these checks should not depend on the model's confidence.

For the vLLM pilot, these rules preserve the 26.04 / 580.142 / Qwen3-8B launch report separately from the 26.04 / 580.173.02 / Qwen3.6 Spark fallback report. They also prevent the unsupported summary “vLLM 26.04 works on R580.” The current two-thread sample demonstrates the method; it is not enough coverage to publish a broad compatibility recommendation.

## Mini-batch: four focused searches and sample records

I ran separate Membrick searches for installation, model-loading memory pressure, driver/model compatibility, and multi-node serving. Search hits were treated as leads. The startup-memory query also surfaced prior session notes, which were excluded as source evidence; the records below cite forum exports only. Two returned threads exceeded Membrick's 8 KiB review limit, so their remaining content was checked in the local export before drafting.

### Example A — Clear cached pages before loading a large model

**Kind:** Troubleshooting workaround  
**Statement:** On DGX Spark unified memory, cached safetensors pages may compete with model tensor allocations during Hugging Face model loading. One reply recommends clearing the page cache immediately before `from_pretrained()`; another Spark user reports that this workaround worked, while noting it treats the symptom.  
**Scope:** Original report: DGX Spark, `nvcr.io/nvidia/pytorch:26.01-py3`, Transformers 5.2.0, PyTorch 2.10.0, `Qwen/Qwen3-Coder-Next-FP8`. The reported workaround’s confirming reply describes a separate Qwen3.5 122B NVFP4 workload.  
**Status:** Reported workaround; not confirmed by the original poster for the exact Qwen3-Coder-Next setup. Do not present it as a general fix or as vLLM-specific advice.  
**Evidence:** [Forum posts #1, #5, and #6](https://forums.developer.nvidia.com/t/361990); Membrick source hash `7ffaa4b0dfe69c8dfc8dd6dc9c737b969ededd43d1e0566dbb2bd3a04f6e6303`. The 9,308-byte source was truncated in `review_search`; the full local export was checked.

### Example B — A two-Spark playbook warning with a reported workaround

**Kind:** Troubleshooting  
**Statement:** A two-Spark Ray cluster showed two GPUs in cluster resources, but vLLM 0.20.1 development build still warned that tensor-parallel size 2 exceeded the one GPU available to the local placement group. The NVIDIA respondent suggested trying the community `spark-vllm-docker` container; the original poster replied that it worked.  
**Scope:** Llama 3.3 70B, `--tensor-parallel-size 2`, Ray executor, two Spark nodes, NVIDIA stacked-Sparks playbook.  
**Status:** Single-user reported resolution after changing to the community container. The thread does not identify which container setting fixed placement or confirm performance, so retain the exact reported workaround and do not infer a general Ray configuration rule.  
**Evidence:** [Forum posts #1–#3](https://forums.developer.nvidia.com/t/374812); Membrick source hash `cd8edfa3762617f0f7e6eb90a019383cb302d63b76de07d3f384183b28a57d53`. The 10,325-byte source was truncated in `review_search`; the full local export was checked.

### Example C — Keep the vLLM 26.04 result scoped to its tested driver and model

**Kind:** Compatibility report  
**Statement:** One tester reports that the NVIDIA vLLM 26.04 container enabled CUDA forward compatibility with compatibility driver 595.58.03 over kernel driver 580.142, and `vllm serve Qwen/Qwen3-8B` reached application startup.  
**Scope:** Release 26.04 / vLLM 0.19.0, kernel driver 580.142, CUDA compatibility driver 595.58.03, Qwen3-8B; the tester's hardware and OS are unstated.  
**Status:** One successful launch report. This does not establish compatibility on every R580 driver or Spark system.  
**Evidence:** [Forum post #5](https://forums.developer.nvidia.com/t/368935/5); Membrick source hash `a319d60140aee5b85a81552f78fa6c29dedfc0d8c2d312de19a7256eda702ec2`; 7,058 bytes, not truncated.

### Held example — Host build instructions are not yet a stable recipe

The installation search returned a thread with host-side vLLM build instructions and a reply that building from `main` inside the NVIDIA container worked. Later replies describe missing development packages, dependency resolution changes, and a CUTLASS symbol issue. A follow-up reports a successful build after changing the architecture target and applying a patch from an unmerged pull request. This is useful, scoped build know-how, but not a stable current installation recipe: preserve the exact build context and patch requirement, and re-test before recommending it broadly. **Evidence:** [Forum posts #2–#10](https://forums.developer.nvidia.com/t/348801); Membrick source hash `b5c6e96279256effbe983e303f67d89466289e558cc7464b62c5d7a8bec15148`. The 9,001-byte review was truncated; the full local export was checked.

These examples show three different outcomes from the same workflow: a bounded workaround, a single-user reported resolution, and a successful compatibility report with missing hardware scope. They should remain separate records with separate evidence labels.

## Proposed Membrick design

### Use existing capabilities for discovery

Membrick already supports topic search and can hydrate search hits into `SourceDocument` records through `review_search`. The `SourceDocument` stores the source text and metadata, including `doc_id`, source, headings, links, timestamp, and filters. See [`SourceDocument`](https://github.com/RooAGI/Membrick/blob/main/src/source.rs#L45) and [source-document hydration in `review_search`](https://github.com/RooAGI/Membrick/blob/main/src/memory_api/review.rs#L474).

### Add paginated source reading

Search results provide relevant excerpts capped at 4,000 characters. `review_search` returns up to 8 KiB of each source document and flags truncated content. Long forum threads can therefore require a direct, paginated source-document read. Add an operation keyed by `doc_id` and the expected source hash, with an offset or continuation token, so each page is guaranteed to come from the same source revision.

### Add a typed know-how record

The current MCP surface has search and memory-write operations, but no typed know-how creation operation. A know-how record should be distinct from the source `SourceDocument` and include:

- kind, title, statement, symptoms, steps, outcome, and caveats;
- hardware/software scope and whether scope was inferred;
- status and confirmation/staff signals;
- evidence entries containing source `doc_id`, source hash, and post number/role;
- links to related, superseding, or conflicting know-how records.

Validate evidence references against stored source documents before saving. Keep source text immutable; update a know-how record when new evidence changes the conclusion.

## Avoiding an LLM call for every post

The selection boundary is the source document or thread, not each forum post. The practical flow is:

- use Membrick retrieval to find candidate documents;
- use human triage to select high-value threads;
- have a person write the record or use a model only on selected complete threads;
- validate schema, scope, and evidence links in code;
- use Membrick search again for duplicate, corroborating, and contradictory records.

This reduces model usage while preserving useful coverage through retrieval. It does not guarantee a complete curated knowledge base unless the candidate search process is measured against a known set or periodically audited.

## Recommended pilot

Continue with `inference-vllm` and run four subtopic searches: installation, startup memory/OOM, model/driver compatibility, and multi-node serving. For compatibility, retain both threads 368935 and 379168 as the starting evidence set and search for additional reports of the same image/driver/model combinations. For each search, retrieve the `SourceDocument`s, deduplicate by ID, and label relevance. Draft a small set of know-how records from selected sources, including single-report items with explicit evidence labels. Review whether search results surfaced relevant sources and whether each record preserves exact scope and provenance before expanding to another topic.
