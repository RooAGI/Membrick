//! Bounded operational telemetry for long-running server processes.
//!
//! This module stores bounded query aggregates, detailed MCP tool history,
//! and provider lifecycle metadata. MCP entries are capped at one MiB; when a
//! request or response exceeds that limit the ledger stores omission markers
//! while preserving the trace identity and call metadata.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::{mpsc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const BUCKET_WIDTH_MS: u64 = 5_000;
const MAX_BUCKETS: usize = 120;
const QUERY_EVENT_QUEUE_CAPACITY: usize = 1_024;
const LATENCY_BOUNDARIES_MS: [u64; 12] = [1, 2, 5, 10, 20, 50, 100, 250, 500, 1_000, 5_000, 30_000];
const PROVIDER_TELEMETRY_DIR: &str = "provider-telemetry";
pub const MAX_PROVIDER_TELEMETRY_EVENTS: usize = 500;
const QUERY_TELEMETRY_FILE: &str = "query-telemetry.json";
const MCP_CALL_HISTORY_FILE: &str = "mcp-call-history.jsonl";
const MAX_MCP_CALL_HISTORY_RECORD_BYTES: usize = 16 * 1024 * 1024;

struct BoundedJsonWriter {
    bytes: Vec<u8>,
    limit: usize,
}

impl Write for BoundedJsonWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let remaining = self.limit.saturating_sub(self.bytes.len());
        if bytes.len() > remaining {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "MCP call history record exceeds byte limit",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn serialize_mcp_call_record_bounded(record: &mut McpCallRecord) -> Vec<u8> {
    let serialize = |record: &McpCallRecord| {
        let mut writer = BoundedJsonWriter {
            bytes: Vec::with_capacity(16 * 1024),
            limit: MAX_MCP_CALL_HISTORY_RECORD_BYTES,
        };
        serde_json::to_writer(&mut writer, record)
            .ok()
            .map(|()| writer.bytes)
    };

    if let Some(bytes) = serialize(record) {
        return bytes;
    }

    record.arguments = serde_json::json!({
        "status":"omitted",
        "reason":"record exceeded history byte limit"
    });
    record.response = serde_json::json!({
        "status":"omitted",
        "reason":"record exceeded history byte limit"
    });
    if let Some(bytes) = serialize(record) {
        return bytes;
    }

    record.request_id = None;
    record.session_id = None;
    record.provider.truncate(64);
    record.tool.truncate(64);
    serialize(record).expect("minimal MCP history record fits byte limit")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpCallRecord {
    /// Stable identifier for correlating this request and its result across
    /// maintenance/replay tooling. Older history entries do not have one.
    #[serde(default)]
    pub trace_id: Option<String>,
    pub timestamp_ms: u64,
    pub provider: String,
    pub tool: String,
    /// Session explicitly named by the caller. An omitted id remains absent;
    /// consumers must not infer session membership from nearby timestamps.
    #[serde(default)]
    pub session_id: Option<String>,
    /// `explicit` when `session_id` was supplied, otherwise `absent`.
    /// Future resident-side attribution can add `inferred` without changing
    /// the call-history schema again.
    #[serde(default = "default_session_attribution")]
    pub session_attribution: String,
    #[serde(default)]
    pub request_id: Option<serde_json::Value>,
    pub arguments: serde_json::Value,
    pub duration_ms: u64,
    pub error: bool,
    pub response: serde_json::Value,
}

/// Persist a bounded request and response record for an MCP memory tool call.
/// The ledger is local to the workspace and append-only.
pub fn record_mcp_call(
    project_root: &Path,
    provider: &str,
    tool: &str,
    request_id: Option<serde_json::Value>,
    arguments: &serde_json::Value,
    duration_ms: u64,
    error: bool,
    response: serde_json::Value,
) -> Result<()> {
    let (timestamp_ms, trace_id) = mcp_trace_id_for_call(provider, tool, &request_id, arguments)?;
    record_mcp_call_with_trace_id(
        project_root,
        provider,
        tool,
        request_id,
        arguments,
        duration_ms,
        error,
        response,
        timestamp_ms,
        trace_id,
    )
}

/// Generate the trace/search ID before the provider response is returned, so
/// the same ID can be included in the search result and its history record.
pub(crate) fn mcp_trace_id_for_call(
    provider: &str,
    tool: &str,
    request_id: &Option<serde_json::Value>,
    arguments: &serde_json::Value,
) -> Result<(u64, String)> {
    let timestamp_ms = now_ms();
    let record = McpCallRecord {
        trace_id: None,
        timestamp_ms,
        provider: provider.to_string(),
        tool: tool.to_string(),
        session_id: explicit_session_id(arguments),
        session_attribution: if explicit_session_id(arguments).is_some() {
            "explicit".to_string()
        } else {
            "absent".to_string()
        },
        request_id: request_id.clone(),
        arguments: arguments.clone(),
        duration_ms: 0,
        error: false,
        response: serde_json::Value::Null,
    };
    Ok((timestamp_ms, mcp_trace_id(&record)?))
}

/// Append a call using a trace ID already exposed in its provider-facing
/// response. Other callers can use `record_mcp_call`, which creates the ID.
pub(crate) fn record_mcp_call_with_trace_id(
    project_root: &Path,
    provider: &str,
    tool: &str,
    request_id: Option<serde_json::Value>,
    arguments: &serde_json::Value,
    duration_ms: u64,
    error: bool,
    response: serde_json::Value,
    timestamp_ms: u64,
    trace_id: String,
) -> Result<()> {
    let session_id = explicit_session_id(arguments);
    let attribution = if session_id.is_some() {
        "explicit"
    } else {
        "absent"
    };
    record_mcp_call_with_trace_id_and_attribution(
        project_root,
        provider,
        tool,
        request_id,
        arguments,
        duration_ms,
        error,
        response,
        timestamp_ms,
        trace_id,
        session_id,
        attribution,
    )
}

pub(crate) fn record_mcp_call_with_trace_id_and_attribution(
    project_root: &Path,
    provider: &str,
    tool: &str,
    request_id: Option<serde_json::Value>,
    arguments: &serde_json::Value,
    duration_ms: u64,
    error: bool,
    response: serde_json::Value,
    timestamp_ms: u64,
    trace_id: String,
    session_id: Option<String>,
    session_attribution: &str,
) -> Result<()> {
    let root = project_root.join(".membrick");
    fs::create_dir_all(&root)?;
    let lock = acquire_telemetry_lock(&root.join("mcp-call-history.lock"))?;
    let path = root.join(MCP_CALL_HISTORY_FILE);
    let record = McpCallRecord {
        trace_id: None,
        timestamp_ms,
        provider: provider.to_string(),
        tool: tool.to_string(),
        session_id,
        session_attribution: session_attribution.to_string(),
        request_id,
        arguments: arguments.clone(),
        duration_ms,
        error,
        response,
    };
    let mut record = record;
    record.trace_id = Some(trace_id);
    let mut line = serialize_mcp_call_record_bounded(&mut record);
    line.push(b'\n');
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(&line)?;
    drop(lock);
    Ok(())
}

fn default_session_attribution() -> String {
    "absent".to_string()
}

fn explicit_session_id(arguments: &serde_json::Value) -> Option<String> {
    arguments
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|session_id| !session_id.is_empty())
        .map(str::to_string)
}

fn mcp_trace_id(record: &McpCallRecord) -> Result<String> {
    let mut digest = Sha256::new();
    digest.update(b"membrick-mcp-trace-v1\0");
    digest.update(record.timestamp_ms.to_le_bytes());
    digest.update(record.provider.as_bytes());
    digest.update([0]);
    digest.update(record.tool.as_bytes());
    digest.update([0]);
    if let Some(request_id) = &record.request_id {
        digest.update(serde_json::to_vec(request_id)?);
    } else {
        digest.update(serde_json::to_vec(&record.arguments)?);
    }
    let encoded = digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(format!("mcp:{encoded}"))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderLifecycleStatus {
    pub schema_version: u8,
    pub provider: String,
    pub last_event: Option<String>,
    pub last_seen_ms: u64,
    pub events_total: u64,
    pub sessions_started: u64,
    pub sessions_ended: u64,
    pub sessions_active: u64,
    pub retrieval_events: u64,
    pub capture_events: u64,
    #[serde(default)]
    pub events: Vec<ProviderLifecycleEvent>,
    /// Cumulative counters updated on every recorded event. Unlike `events`,
    /// they are not limited to the retained window, so busy providers whose
    /// window is filled by retrieval and tool bursts still report prompts,
    /// stops and token usage. Ledgers written before these counters existed
    /// load with zeroed counters and `since_ms == 0`.
    #[serde(default)]
    pub counters: ProviderLifecycleCounters,
    /// The newest prompt, stop, usage and subagent events, kept apart from
    /// `events` so retrieval and tool bursts cannot evict them.
    #[serde(default)]
    pub milestone_events: Vec<ProviderLifecycleEvent>,
}

const MAX_PROVIDER_MILESTONE_EVENTS: usize = 50;
const MAX_OPEN_PROMPT_SESSIONS: usize = 256;
const MAX_OPEN_SUBAGENTS: usize = 256;
/// Upper bounds of the prompt-to-stop duration histogram buckets. The last
/// bucket (index `len()`) collects everything slower.
pub const CYCLE_DURATION_BOUNDARIES_MS: [u64; 14] = [
    1_000, 2_000, 5_000, 10_000, 20_000, 30_000, 60_000, 120_000, 300_000, 600_000, 1_200_000,
    1_800_000, 3_600_000, 7_200_000,
];

/// Cumulative per-provider counters. Every field defaults so older ledgers
/// keep loading.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderLifecycleCounters {
    /// When counting started for this ledger (first event recorded by a
    /// build that has counters), in Unix milliseconds. Zero means never.
    pub since_ms: u64,
    /// Prompt submissions (`UserPromptSubmit`, `BeforeAgent`).
    pub prompts: u64,
    /// Turn stops (`Stop`, `AfterAgent`).
    pub stops: u64,
    /// Tool invocations (`PreToolUse`, `BeforeTool`).
    pub tool_calls: u64,
    /// Tool results (`PostToolUse`, `AfterTool`).
    pub tool_results: u64,
    /// Failed tool results (`PostToolUseFailure`).
    pub tool_failures: u64,
    pub subagent_starts: u64,
    pub subagent_stops: u64,
    /// Events that carried any token usage.
    pub usage_events: u64,
    /// Uncached input tokens. Disjoint from the two cache counters.
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    /// Sum of provider-reported `total_tokens`, which excludes cache tokens.
    pub reported_total_tokens: u64,
    /// Prompts followed by a stop in the same session.
    pub cycles_completed: u64,
    pub cycle_duration_sum_ms: u64,
    pub cycle_duration_max_ms: u64,
    /// Histogram over `CYCLE_DURATION_BOUNDARIES_MS` plus one overflow bucket.
    pub cycle_duration_buckets: Vec<u64>,
    /// Open prompt start time by opaque session key, bounded to
    /// `MAX_OPEN_PROMPT_SESSIONS` entries.
    pub open_prompts: std::collections::BTreeMap<String, u64>,
    /// Subagents started with no stop yet, by agent id, bounded to
    /// `MAX_OPEN_SUBAGENTS` entries. Starts without an agent id are not kept.
    pub open_subagents: std::collections::BTreeMap<String, OpenSubagent>,
    /// Open subagents replaced by a later start of the same agent id, which
    /// happens when a run dies (for example on an API error) and is resumed.
    pub subagent_interrupted: u64,
    /// Open subagents whose session ended before they sent a stop.
    pub subagent_ended_without_stop: u64,
    /// Agent ids that SessionEnd closed, with the time it did, bounded to
    /// `MAX_OPEN_SUBAGENTS`. A stop that arrives later moves its run from
    /// ended-without-stop to stopped, so no run is counted twice.
    pub ended_subagents: std::collections::BTreeMap<String, u64>,
}

/// A subagent run that has started and not stopped.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenSubagent {
    pub started_ms: u64,
    /// Latest event carrying this agent id, so readers can tell a quiet run.
    pub last_seen_ms: u64,
    pub session_key: String,
}

impl ProviderLifecycleCounters {
    /// Estimated prompt-to-stop duration quantile, as the upper bound of the
    /// histogram bucket that holds it. `None` when no cycle completed.
    pub fn cycle_duration_quantile_ms(&self, quantile: f64) -> Option<u64> {
        let total = self.cycle_duration_buckets.iter().sum::<u64>();
        if total == 0 {
            return None;
        }
        let target = ((total as f64 * quantile).ceil() as u64).max(1);
        let mut seen = 0_u64;
        for (index, count) in self.cycle_duration_buckets.iter().enumerate() {
            seen = seen.saturating_add(*count);
            if seen >= target {
                return Some(
                    CYCLE_DURATION_BOUNDARIES_MS
                        .get(index)
                        .copied()
                        .unwrap_or(self.cycle_duration_max_ms),
                );
            }
        }
        None
    }

    fn record(&mut self, event: &ProviderLifecycleEvent) {
        if self.since_ms == 0 {
            self.since_ms = event.timestamp_ms;
        }
        match event.event.as_str() {
            "UserPromptSubmit" | "BeforeAgent" => {
                self.prompts = self.prompts.saturating_add(1);
                self.open_prompts
                    .insert(event.session_key.clone(), event.timestamp_ms);
                while self.open_prompts.len() > MAX_OPEN_PROMPT_SESSIONS {
                    let oldest = self
                        .open_prompts
                        .iter()
                        .min_by_key(|(_, started)| **started)
                        .map(|(key, _)| key.clone());
                    match oldest {
                        Some(key) => self.open_prompts.remove(&key),
                        None => break,
                    };
                }
            }
            "Stop" | "AfterAgent" => {
                self.stops = self.stops.saturating_add(1);
                self.close_cycle(event);
            }
            // A session that ends without a stop abandoned its turn: drop the
            // open prompt without counting a completed cycle.
            "SessionEnd" => {
                self.open_prompts.remove(&event.session_key);
                let ended = self
                    .open_subagents
                    .iter()
                    .filter(|(_, open)| open.session_key == event.session_key)
                    .map(|(agent_id, _)| agent_id.clone())
                    .collect::<Vec<_>>();
                for agent_id in ended {
                    self.open_subagents.remove(&agent_id);
                    self.ended_subagents.insert(agent_id, event.timestamp_ms);
                    self.subagent_ended_without_stop =
                        self.subagent_ended_without_stop.saturating_add(1);
                }
                while self.ended_subagents.len() > MAX_OPEN_SUBAGENTS {
                    let oldest = self
                        .ended_subagents
                        .iter()
                        .min_by_key(|(_, ended)| **ended)
                        .map(|(key, _)| key.clone());
                    match oldest {
                        Some(key) => self.ended_subagents.remove(&key),
                        None => break,
                    };
                }
            }
            "PreToolUse" | "BeforeTool" => self.tool_calls = self.tool_calls.saturating_add(1),
            "PostToolUse" | "AfterTool" => self.tool_results = self.tool_results.saturating_add(1),
            "PostToolUseFailure" => {
                self.tool_results = self.tool_results.saturating_add(1);
                self.tool_failures = self.tool_failures.saturating_add(1);
            }
            "SubagentStart" => {
                self.subagent_starts = self.subagent_starts.saturating_add(1);
                self.open_subagent(event);
            }
            "SubagentStop" => {
                self.subagent_stops = self.subagent_stops.saturating_add(1);
                if let Some(agent_id) = &event.agent_id {
                    if self.open_subagents.remove(agent_id).is_none()
                        && self.ended_subagents.remove(agent_id).is_some()
                    {
                        self.subagent_ended_without_stop =
                            self.subagent_ended_without_stop.saturating_sub(1);
                    }
                }
            }
            _ => {}
        }
        if let Some(open) = event
            .agent_id
            .as_ref()
            .and_then(|agent_id| self.open_subagents.get_mut(agent_id))
        {
            open.last_seen_ms = open.last_seen_ms.max(event.timestamp_ms);
        }
        let usage = [
            event.input_tokens,
            event.output_tokens,
            event.total_tokens,
            event.cache_creation_input_tokens,
            event.cache_read_input_tokens,
        ];
        if usage.iter().any(Option::is_some) {
            self.usage_events = self.usage_events.saturating_add(1);
            let add = |total: &mut u64, value: Option<u64>| {
                *total = total.saturating_add(value.unwrap_or(0));
            };
            add(&mut self.input_tokens, event.input_tokens);
            add(&mut self.output_tokens, event.output_tokens);
            add(&mut self.reported_total_tokens, event.total_tokens);
            add(
                &mut self.cache_creation_input_tokens,
                event.cache_creation_input_tokens,
            );
            add(
                &mut self.cache_read_input_tokens,
                event.cache_read_input_tokens,
            );
        }
    }

    fn open_subagent(&mut self, event: &ProviderLifecycleEvent) {
        let Some(agent_id) = event.agent_id.clone() else {
            return;
        };
        let open = OpenSubagent {
            started_ms: event.timestamp_ms,
            last_seen_ms: event.timestamp_ms,
            session_key: event.session_key.clone(),
        };
        self.ended_subagents.remove(&agent_id);
        if self.open_subagents.insert(agent_id, open).is_some() {
            self.subagent_interrupted = self.subagent_interrupted.saturating_add(1);
        }
        while self.open_subagents.len() > MAX_OPEN_SUBAGENTS {
            let oldest = self
                .open_subagents
                .iter()
                .min_by_key(|(_, open)| open.last_seen_ms)
                .map(|(key, _)| key.clone());
            match oldest {
                Some(key) => self.open_subagents.remove(&key),
                None => break,
            };
        }
    }

    fn close_cycle(&mut self, event: &ProviderLifecycleEvent) {
        let Some(started) = self.open_prompts.remove(&event.session_key) else {
            return;
        };
        let duration = event.timestamp_ms.saturating_sub(started);
        self.cycles_completed = self.cycles_completed.saturating_add(1);
        self.cycle_duration_sum_ms = self.cycle_duration_sum_ms.saturating_add(duration);
        self.cycle_duration_max_ms = self.cycle_duration_max_ms.max(duration);
        let buckets = CYCLE_DURATION_BOUNDARIES_MS.len() + 1;
        if self.cycle_duration_buckets.len() != buckets {
            self.cycle_duration_buckets.resize(buckets, 0);
        }
        let index = CYCLE_DURATION_BOUNDARIES_MS
            .iter()
            .position(|bound| duration <= *bound)
            .unwrap_or(CYCLE_DURATION_BOUNDARIES_MS.len());
        self.cycle_duration_buckets[index] = self.cycle_duration_buckets[index].saturating_add(1);
    }
}

fn is_milestone_event(event_name: &str) -> bool {
    matches!(
        event_name,
        "UserPromptSubmit"
            | "BeforeAgent"
            | "Stop"
            | "AfterAgent"
            | "TurnUsage"
            | "SubagentStart"
            | "SubagentStop"
            | "SessionStart"
            | "SessionEnd"
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderLifecycleEvent {
    pub event_id: String,
    pub provider: String,
    pub session_key: String,
    pub event: String,
    pub category: String,
    pub timestamp_ms: u64,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub agent_type: Option<String>,
    #[serde(default)]
    pub turn_id: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_arguments: Option<String>,
    #[serde(default)]
    pub prompt_preview: Option<String>,
    #[serde(default)]
    pub tool_response_preview: Option<String>,
    #[serde(default)]
    pub stop_response_preview: Option<String>,
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
    #[serde(default)]
    pub total_tokens: Option<u64>,
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    pub retrieval_query_preview: Option<String>,
    #[serde(default)]
    pub retrieval_latency_ms: Option<u64>,
    #[serde(default)]
    pub retrieved_memory_count: Option<u64>,
    #[serde(default)]
    pub retrieved_memory_details: Option<String>,
}

pub fn record_memory_retrieval(
    project_root: &Path,
    provider: &str,
    session_id: &str,
    query: &str,
    latency_ms: u64,
    memories: &[serde_json::Value],
) -> Result<()> {
    record_provider_lifecycle_event(
        project_root,
        provider,
        session_id,
        "MemoryRetrieved",
        &serde_json::json!({
            "query": query,
            "retrieval_latency_ms": latency_ms,
            "retrieved_memory_count": memories.len() as u64,
            "retrieved_memory_details": memories,
        }),
    )
}

pub fn record_provider_lifecycle_event(
    project_root: &Path,
    provider: &str,
    session_id: &str,
    event_name: &str,
    payload: &serde_json::Value,
) -> Result<()> {
    let root = project_root.join(".membrick").join(PROVIDER_TELEMETRY_DIR);
    fs::create_dir_all(&root)?;
    let lock = acquire_telemetry_lock(&root.join(format!("{provider}.lock")))?;
    let path = root.join(format!("{provider}.json"));
    let mut status = if path.exists() {
        serde_json::from_str::<ProviderLifecycleStatus>(&fs::read_to_string(&path)?)
            .unwrap_or_default()
    } else {
        ProviderLifecycleStatus {
            schema_version: 1,
            provider: provider.to_string(),
            ..ProviderLifecycleStatus::default()
        }
    };
    let category = lifecycle_category(event_name);
    let (
        tool_name,
        tool_arguments,
        prompt_preview,
        tool_response_preview,
        stop_response_preview,
        agent_id,
        agent_type,
        turn_id,
        input_tokens,
        output_tokens,
        total_tokens,
        cache_creation_input_tokens,
        cache_read_input_tokens,
        retrieval_query_preview,
        retrieval_latency_ms,
        retrieved_memory_count,
        retrieved_memory_details,
    ) = event_details(provider, event_name, payload);
    status.provider = provider.to_string();
    status.last_event = Some(event_name.to_string());
    status.last_seen_ms = now_ms();
    status.events_total = status.events_total.saturating_add(1);
    match category {
        "session" if event_name.eq_ignore_ascii_case("SessionStart") => {
            status.sessions_started = status.sessions_started.saturating_add(1);
            status.sessions_active = status.sessions_active.saturating_add(1);
        }
        "session" if event_name.eq_ignore_ascii_case("SessionEnd") => {
            status.sessions_ended = status.sessions_ended.saturating_add(1);
            status.sessions_active = status.sessions_active.saturating_sub(1);
        }
        "retrieval" => status.retrieval_events = status.retrieval_events.saturating_add(1),
        "capture" => status.capture_events = status.capture_events.saturating_add(1),
        _ => {}
    }
    let event = ProviderLifecycleEvent {
        event_id: format!("{}-{}", status.last_seen_ms, status.events_total),
        provider: provider.to_string(),
        session_key: opaque_session_key(session_id),
        event: event_name.to_string(),
        category: category.to_string(),
        timestamp_ms: status.last_seen_ms,
        agent_id,
        agent_type,
        turn_id,
        tool_name,
        tool_arguments,
        prompt_preview,
        tool_response_preview,
        stop_response_preview,
        input_tokens,
        output_tokens,
        total_tokens,
        cache_creation_input_tokens,
        cache_read_input_tokens,
        retrieval_query_preview,
        retrieval_latency_ms,
        retrieved_memory_count,
        retrieved_memory_details,
    };
    status.counters.record(&event);
    if is_milestone_event(event_name) {
        status.milestone_events.push(event.clone());
        if status.milestone_events.len() > MAX_PROVIDER_MILESTONE_EVENTS {
            let keep_from = status.milestone_events.len() - MAX_PROVIDER_MILESTONE_EVENTS;
            status.milestone_events.drain(..keep_from);
        }
    }
    status.events.push(event);
    if status.events.len() > MAX_PROVIDER_TELEMETRY_EVENTS {
        let keep_from = status.events.len() - MAX_PROVIDER_TELEMETRY_EVENTS;
        status.events.drain(..keep_from);
    }
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(&status)?)?;
    fs::rename(temporary, path)?;
    drop(lock);
    Ok(())
}

/// Extract and normalize provider usage into the telemetry schema.
///
/// Provider adapters may pass either a lifecycle payload or a transcript line;
/// keeping this here ensures both paths use identical accounting semantics.
pub fn normalize_usage(provider: &str, value: &serde_json::Value) -> Option<serde_json::Value> {
    fn visit(provider: &str, value: &serde_json::Value) -> Option<serde_json::Value> {
        match value {
            serde_json::Value::Object(object) => {
                for key in [
                    "usage",
                    "token_usage",
                    "tokenUsage",
                    "last_token_usage",
                    "total_token_usage",
                ] {
                    if let Some(candidate) = object.get(key) {
                        if let Some(normalized) = normalize_object(provider, candidate, object) {
                            return Some(normalized);
                        }
                    }
                }
                object.values().find_map(|value| visit(provider, value))
            }
            serde_json::Value::Array(values) => {
                values.iter().find_map(|value| visit(provider, value))
            }
            _ => None,
        }
    }

    visit(provider, value)
}

fn normalize_object(
    provider: &str,
    value: &serde_json::Value,
    parent: &serde_json::Map<String, serde_json::Value>,
) -> Option<serde_json::Value> {
    let object = value.as_object()?;
    let number = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| object.get(*key).and_then(serde_json::Value::as_u64))
            .or_else(|| {
                keys.iter()
                    .find_map(|key| parent.get(*key).and_then(serde_json::Value::as_u64))
            })
    };
    let raw_input = number(&["input_tokens", "inputTokens", "prompt_tokens"]);
    let output = number(&["output_tokens", "outputTokens", "completion_tokens"]);
    let cache_creation = number(&[
        "cache_creation_input_tokens",
        "cacheCreationInputTokens",
        "cache_write_input_tokens",
        "cacheWriteInputTokens",
    ]);
    let cache_read = number(&[
        "cache_read_input_tokens",
        "cacheReadInputTokens",
        "cached_input_tokens",
    ]);
    let input_includes_cache = provider.eq_ignore_ascii_case("codex")
        && (object.contains_key("cached_input_tokens")
            || object.contains_key("cache_write_input_tokens")
            || parent.contains_key("cached_input_tokens")
            || parent.contains_key("cache_write_input_tokens"));
    let input = raw_input.map(|value| {
        if input_includes_cache {
            value.saturating_sub(
                cache_read
                    .unwrap_or(0)
                    .saturating_add(cache_creation.unwrap_or(0)),
            )
        } else {
            value
        }
    });
    let total = number(&["total_tokens", "totalTokens"])
        .or_else(|| raw_input.map(|value| value + output.unwrap_or(0)));
    if input.is_none()
        && output.is_none()
        && cache_creation.is_none()
        && cache_read.is_none()
        && total.is_none()
    {
        return None;
    }
    Some(serde_json::json!({
        "input_tokens": input,
        "output_tokens": output,
        "cache_creation_input_tokens": cache_creation,
        "cache_read_input_tokens": cache_read,
        "total_tokens": total,
        "source": "hook-payload"
    }))
}

fn event_details(
    provider: &str,
    event_name: &str,
    payload: &serde_json::Value,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<u64>,
    Option<u64>,
    Option<u64>,
    Option<u64>,
    Option<u64>,
    Option<String>,
    Option<u64>,
    Option<u64>,
    Option<String>,
) {
    let object = payload.as_object();
    let value_for =
        |keys: &[&str]| object.and_then(|object| keys.iter().find_map(|key| object.get(*key)));
    let string_for = |keys: &[&str]| value_for(keys).and_then(|value| value.as_str());
    let identity_for = |keys: &[&str]| string_for(keys).map(|value| truncate_text(value, 160));
    let tool_event = matches!(
        event_name,
        "PreToolUse" | "PostToolUse" | "PostToolUseFailure" | "BeforeTool"
    );
    let prompt_event = matches!(
        event_name,
        "UserPromptSubmit" | "UserPromptExpansion" | "BeforeAgent"
    );
    let response_event = matches!(
        event_name,
        "PostToolUse" | "PostToolUseFailure" | "AfterTool"
    );
    let stop_event = matches!(
        event_name,
        "Stop" | "AfterAgent" | "SessionEnd" | "SubagentStop"
    );
    let tool_name = tool_event
        .then(|| string_for(&["tool_name", "toolName", "name"]))
        .flatten()
        .map(|value| truncate_text(value, 160));
    let tool_arguments = tool_event
        .then(|| value_for(&["tool_input", "toolInput", "arguments", "args"]))
        .flatten()
        .and_then(preview_value);
    let prompt_preview = prompt_event
        .then(|| value_for(&["prompt", "user_prompt", "text"]))
        .flatten()
        .and_then(preview_value);
    let tool_response_preview = response_event
        .then(|| value_for(&["tool_response", "toolResponse", "tool_output", "response"]))
        .flatten()
        .and_then(preview_value);
    let stop_response_preview = stop_event
        .then(|| value_for(&["final_response", "response", "output", "reason"]))
        .flatten()
        .and_then(preview_value);
    let retrieval_query_preview = (event_name == "MemoryRetrieved")
        .then(|| string_for(&["query"]))
        .flatten()
        .map(|value| truncate_text(value, 512));
    let retrieval_latency_ms = (event_name == "MemoryRetrieved")
        .then(|| value_for(&["retrieval_latency_ms"]).and_then(serde_json::Value::as_u64))
        .flatten();
    let retrieved_memory_count = (event_name == "MemoryRetrieved")
        .then(|| value_for(&["retrieved_memory_count"]).and_then(serde_json::Value::as_u64))
        .flatten();
    let retrieved_memory_details = (event_name == "MemoryRetrieved")
        .then(|| value_for(&["retrieved_memory_details"]))
        .flatten()
        .and_then(preview_value);
    let usage = normalize_usage(provider, payload);
    let usage = usage.as_ref().and_then(serde_json::Value::as_object);
    let usage_number = |keys: &[&str]| {
        usage
            .and_then(|usage| {
                keys.iter()
                    .find_map(|key| usage.get(*key).and_then(serde_json::Value::as_u64))
            })
            .or_else(|| {
                object.and_then(|object| {
                    keys.iter()
                        .find_map(|key| object.get(*key).and_then(serde_json::Value::as_u64))
                })
            })
    };
    (
        tool_name,
        tool_arguments,
        prompt_preview,
        tool_response_preview,
        stop_response_preview,
        identity_for(&["agent_id", "agentId", "subagent_id", "subagentId"]),
        identity_for(&["agent_type", "agentType", "subagent_type", "subagentType"]),
        identity_for(&["turn_id", "turnId"]),
        usage_number(&["input_tokens", "inputTokens", "prompt_tokens"]),
        usage_number(&["output_tokens", "outputTokens", "completion_tokens"]),
        usage_number(&["total_tokens", "totalTokens"]),
        usage_number(&["cache_creation_input_tokens"]),
        usage_number(&["cache_read_input_tokens"]),
        retrieval_query_preview,
        retrieval_latency_ms,
        retrieved_memory_count,
        retrieved_memory_details,
    )
}

fn preview_value(value: &serde_json::Value) -> Option<String> {
    let sanitized = sanitize_tool_value(value, None, 0);
    let rendered = match sanitized {
        serde_json::Value::String(value) => value,
        value => serde_json::to_string(&value).ok()?,
    };
    Some(truncate_text(&rendered, 2_048))
}

fn sanitize_tool_value(
    value: &serde_json::Value,
    key: Option<&str>,
    depth: usize,
) -> serde_json::Value {
    if depth > 6 {
        return serde_json::Value::String("[MAX_DEPTH]".to_string());
    }
    if key.is_some_and(is_sensitive_key) {
        return serde_json::Value::String("[REDACTED]".to_string());
    }
    match value {
        serde_json::Value::Object(object) => serde_json::Value::Object(
            object
                .iter()
                .take(40)
                .map(|(key, value)| {
                    (
                        key.clone(),
                        sanitize_tool_value(value, Some(key), depth + 1),
                    )
                })
                .collect(),
        ),
        serde_json::Value::Array(values) => serde_json::Value::Array(
            values
                .iter()
                .take(20)
                .map(|value| sanitize_tool_value(value, None, depth + 1))
                .collect(),
        ),
        serde_json::Value::String(value) => {
            if looks_like_credential(value) {
                serde_json::Value::String("[REDACTED]".to_string())
            } else {
                serde_json::Value::String(truncate_text(value, 512))
            }
        }
        other => other.clone(),
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    [
        "api_key",
        "apikey",
        "authorization",
        "bearer",
        "cookie",
        "credential",
        "password",
        "private_key",
        "secret",
        "token",
    ]
    .iter()
    .any(|needle| key.contains(needle))
}

fn looks_like_credential(value: &str) -> bool {
    ["sk-", "sk_live_", "ghp_", "gho_", "xoxb-", "Bearer "]
        .iter()
        .any(|prefix| value.contains(prefix))
}

fn truncate_text(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes.saturating_sub(3);
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}...", &value[..end])
}

pub fn provider_lifecycle_status(
    project_root: &Path,
    provider: &str,
) -> Result<Option<ProviderLifecycleStatus>> {
    let path = project_root
        .join(".membrick")
        .join(PROVIDER_TELEMETRY_DIR)
        .join(format!("{provider}.json"));
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_str(&fs::read_to_string(path)?)?))
}

pub(crate) fn opaque_session_key(session_id: &str) -> String {
    if session_id.trim().is_empty() {
        return "unknown".to_string();
    }
    let digest = Sha256::digest(session_id.as_bytes());
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn lifecycle_category(event_name: &str) -> &'static str {
    match event_name {
        "MemoryRetrieved" => "retrieval",
        "SessionStart" | "SessionEnd" | "SubagentStart" | "SubagentStop" => "session",
        "PreCompact" | "PostCompact" | "PreCompress" => "compaction",
        "Stop" | "AfterAgent" => "capture",
        "UserPromptSubmit"
        | "UserPromptExpansion"
        | "BeforeAgent"
        | "BeforeModel"
        | "BeforeToolSelection"
        | "PreToolUse"
        | "BeforeTool" => "retrieval",
        _ => "lifecycle",
    }
}

struct TelemetryLock {
    file: fs::File,
}

fn acquire_telemetry_lock(path: &Path) -> Result<TelemetryLock> {
    use fs4::fs_std::FileExt;

    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    for _ in 0..500 {
        if file.try_lock_exclusive()? {
            return Ok(TelemetryLock { file });
        }
        thread::sleep(Duration::from_millis(2));
    }
    anyhow::bail!("timed out waiting for provider telemetry lock")
}

#[derive(Default, Clone, Serialize, Deserialize)]
struct QueryBucket {
    start_ms: u64,
    requests: u64,
    errors: u64,
    empty_results: u64,
    latency_counts: [u64; LATENCY_BOUNDARIES_MS.len() + 1],
}

#[derive(Default, Serialize, Deserialize)]
struct PersistedQueryTelemetry {
    started_at_ms: u64,
    #[serde(default)]
    buckets: VecDeque<QueryBucket>,
}

#[derive(Default)]
struct TelemetryState {
    buckets: VecDeque<QueryBucket>,
}

#[derive(Clone, Copy)]
struct QueryEvent {
    duration_ms: u64,
    error: bool,
    empty_results: bool,
}

#[derive(Clone)]
pub struct OperationalTelemetry {
    started_at_ms: u64,
    state: std::sync::Arc<Mutex<TelemetryState>>,
    query_events: mpsc::SyncSender<QueryEvent>,
}

#[derive(Debug, Clone, Serialize)]
pub struct QuerySeriesPoint {
    pub start_ms: u64,
    pub requests: u64,
    pub errors: u64,
    pub empty_results: u64,
    pub requests_per_second: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct QuerySummary {
    pub requests: u64,
    pub errors: u64,
    pub empty_results: u64,
    pub requests_per_second: f64,
    pub error_rate: f64,
    pub empty_result_rate: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TelemetrySnapshot {
    pub started_at_ms: u64,
    pub window_seconds: u64,
    pub query_summary: QuerySummary,
    pub query_series: Vec<QuerySeriesPoint>,
}

/// Record an aggregate for a query executed by an IndexStore API process.
/// Only bounded counters and latency buckets are persisted; query text is not.
pub fn record_project_query(
    project_root: &Path,
    duration_ms: u64,
    error: bool,
    empty_results: bool,
) -> Result<()> {
    let root = project_root.join(".membrick");
    fs::create_dir_all(&root)?;
    let lock = acquire_telemetry_lock(&root.join("query-telemetry.lock"))?;
    let path = root.join(QUERY_TELEMETRY_FILE);
    let mut ledger = if path.exists() {
        serde_json::from_str::<PersistedQueryTelemetry>(&fs::read_to_string(&path)?)
            .unwrap_or_default()
    } else {
        PersistedQueryTelemetry {
            started_at_ms: now_ms(),
            ..PersistedQueryTelemetry::default()
        }
    };
    let now = now_ms();
    let bucket_start = now - (now % BUCKET_WIDTH_MS);
    if ledger
        .buckets
        .back()
        .is_none_or(|bucket| bucket.start_ms != bucket_start)
    {
        ledger.buckets.push_back(QueryBucket {
            start_ms: bucket_start,
            ..QueryBucket::default()
        });
        while ledger.buckets.len() > MAX_BUCKETS {
            ledger.buckets.pop_front();
        }
    }
    if let Some(bucket) = ledger.buckets.back_mut() {
        bucket.requests = bucket.requests.saturating_add(1);
        bucket.errors = bucket.errors.saturating_add(u64::from(error));
        bucket.empty_results = bucket
            .empty_results
            .saturating_add(u64::from(empty_results));
        let latency_bucket = LATENCY_BOUNDARIES_MS
            .iter()
            .position(|boundary| duration_ms <= *boundary)
            .unwrap_or(LATENCY_BOUNDARIES_MS.len());
        bucket.latency_counts[latency_bucket] =
            bucket.latency_counts[latency_bucket].saturating_add(1);
    }
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(&ledger)?)?;
    fs::rename(temporary, path)?;
    drop(lock);
    Ok(())
}

pub fn project_query_snapshot(project_root: &Path) -> Result<Option<TelemetrySnapshot>> {
    let path = project_root.join(".membrick").join(QUERY_TELEMETRY_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let ledger = serde_json::from_str::<PersistedQueryTelemetry>(&fs::read_to_string(path)?)?;
    let buckets = ledger.buckets.into_iter().collect::<Vec<_>>();
    let query_series = buckets.iter().map(series_point).collect::<Vec<_>>();
    let mut aggregate = QueryBucket::default();
    for bucket in &buckets {
        aggregate.requests = aggregate.requests.saturating_add(bucket.requests);
        aggregate.errors = aggregate.errors.saturating_add(bucket.errors);
        aggregate.empty_results = aggregate.empty_results.saturating_add(bucket.empty_results);
        for (index, count) in bucket.latency_counts.iter().enumerate() {
            aggregate.latency_counts[index] =
                aggregate.latency_counts[index].saturating_add(*count);
        }
    }
    let window_seconds = (buckets.len() as u64 * BUCKET_WIDTH_MS) / 1_000;
    Ok(Some(TelemetrySnapshot {
        started_at_ms: ledger.started_at_ms,
        window_seconds,
        query_summary: summary(&aggregate, window_seconds),
        query_series,
    }))
}

impl OperationalTelemetry {
    pub fn new() -> Self {
        let state = std::sync::Arc::new(Mutex::new(TelemetryState::default()));
        let (query_events, receiver) = mpsc::sync_channel::<QueryEvent>(QUERY_EVENT_QUEUE_CAPACITY);
        let worker_state = std::sync::Arc::clone(&state);
        thread::Builder::new()
            .name("membrick-query-telemetry".to_string())
            .spawn(move || {
                while let Ok(event) = receiver.recv() {
                    record_query_in_state(
                        &worker_state,
                        event.duration_ms,
                        event.error,
                        event.empty_results,
                    );
                }
            })
            .expect("query telemetry worker should start");
        Self {
            started_at_ms: now_ms(),
            state,
            query_events,
        }
    }

    pub fn record_query(&self, duration_ms: u64, error: bool, empty_results: bool) {
        let _ = self.query_events.try_send(QueryEvent {
            duration_ms,
            error,
            empty_results,
        });
    }
}

fn record_query_in_state(
    state: &Mutex<TelemetryState>,
    duration_ms: u64,
    error: bool,
    empty_results: bool,
) {
    let now = now_ms();
    let bucket_start = now - (now % BUCKET_WIDTH_MS);
    let Ok(mut state) = state.lock() else {
        return;
    };
    if state
        .buckets
        .back()
        .is_none_or(|bucket| bucket.start_ms != bucket_start)
    {
        state.buckets.push_back(QueryBucket {
            start_ms: bucket_start,
            ..QueryBucket::default()
        });
        while state.buckets.len() > MAX_BUCKETS {
            state.buckets.pop_front();
        }
    }
    let Some(bucket) = state.buckets.back_mut() else {
        return;
    };
    bucket.requests = bucket.requests.saturating_add(1);
    bucket.errors = bucket.errors.saturating_add(u64::from(error));
    bucket.empty_results = bucket
        .empty_results
        .saturating_add(u64::from(empty_results));
    let latency_bucket = LATENCY_BOUNDARIES_MS
        .iter()
        .position(|boundary| duration_ms <= *boundary)
        .unwrap_or(LATENCY_BOUNDARIES_MS.len());
    bucket.latency_counts[latency_bucket] = bucket.latency_counts[latency_bucket].saturating_add(1);
}

impl OperationalTelemetry {
    pub fn snapshot(&self) -> TelemetrySnapshot {
        let buckets = self
            .state
            .lock()
            .map(|state| state.buckets.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        let query_series = buckets.iter().map(series_point).collect::<Vec<_>>();
        let mut aggregate = QueryBucket::default();
        for bucket in &buckets {
            aggregate.requests = aggregate.requests.saturating_add(bucket.requests);
            aggregate.errors = aggregate.errors.saturating_add(bucket.errors);
            aggregate.empty_results = aggregate.empty_results.saturating_add(bucket.empty_results);
            for (index, count) in bucket.latency_counts.iter().enumerate() {
                aggregate.latency_counts[index] =
                    aggregate.latency_counts[index].saturating_add(*count);
            }
        }
        let window_seconds = (buckets.len() as u64 * BUCKET_WIDTH_MS) / 1_000;
        TelemetrySnapshot {
            started_at_ms: self.started_at_ms,
            window_seconds,
            query_summary: summary(&aggregate, window_seconds),
            query_series,
        }
    }
}

impl Default for OperationalTelemetry {
    fn default() -> Self {
        Self::new()
    }
}

fn series_point(bucket: &QueryBucket) -> QuerySeriesPoint {
    let summary = summary(bucket, BUCKET_WIDTH_MS / 1_000);
    QuerySeriesPoint {
        start_ms: bucket.start_ms,
        requests: bucket.requests,
        errors: bucket.errors,
        empty_results: bucket.empty_results,
        requests_per_second: summary.requests_per_second,
        p50_ms: summary.p50_ms,
        p95_ms: summary.p95_ms,
    }
}

fn summary(bucket: &QueryBucket, window_seconds: u64) -> QuerySummary {
    let requests = bucket.requests;
    let seconds = window_seconds.max(1) as f64;
    QuerySummary {
        requests,
        errors: bucket.errors,
        empty_results: bucket.empty_results,
        requests_per_second: requests as f64 / seconds,
        error_rate: ratio(bucket.errors, requests),
        empty_result_rate: ratio(bucket.empty_results, requests),
        p50_ms: percentile(&bucket.latency_counts, requests, 0.50),
        p95_ms: percentile(&bucket.latency_counts, requests, 0.95),
    }
}

fn percentile(counts: &[u64], total: u64, quantile: f64) -> f64 {
    if total == 0 {
        return 0.0;
    }
    let target = ((total as f64 * quantile).ceil() as u64).max(1);
    let mut seen: u64 = 0;
    for (index, count) in counts.iter().enumerate() {
        seen = seen.saturating_add(*count);
        if seen >= target {
            return if index < LATENCY_BOUNDARIES_MS.len() {
                LATENCY_BOUNDARIES_MS[index] as f64
            } else {
                (LATENCY_BOUNDARIES_MS[LATENCY_BOUNDARIES_MS.len() - 1] * 2) as f64
            };
        }
    }
    0.0
}

fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_directory() -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "membrick-telemetry-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn mcp_call_history_has_trace_identity_and_explicit_session_attribution() {
        let directory = test_directory();
        record_mcp_call(
            &directory,
            "codex",
            "search",
            Some(serde_json::json!(7)),
            &serde_json::json!({"query":"latest CLI","session_id":"session-1"}),
            12,
            false,
            serde_json::json!({"results": []}),
        )
        .unwrap();
        let history =
            fs::read_to_string(directory.join(".membrick/mcp-call-history.jsonl")).unwrap();
        let record: McpCallRecord = serde_json::from_str(history.trim()).unwrap();
        assert!(record
            .trace_id
            .as_deref()
            .is_some_and(|id| id.starts_with("mcp:")));
        assert_eq!(record.session_id.as_deref(), Some("session-1"));
        assert_eq!(record.session_attribution, "explicit");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn mcp_call_history_bounds_large_responses() {
        let directory = test_directory();
        record_mcp_call(
            &directory,
            "codex",
            "review_search",
            None,
            &serde_json::json!({"page_size": 10}),
            12,
            false,
            serde_json::json!({"documents": "x".repeat(20 * 1024 * 1024)}),
        )
        .unwrap();

        let history = fs::read(directory.join(".membrick/mcp-call-history.jsonl")).unwrap();
        assert!(history.len() <= 16 * 1024 * 1024);
        let record: McpCallRecord = serde_json::from_slice(&history[..history.len() - 1]).unwrap();
        assert_eq!(record.response["status"], "omitted");
        assert!(record.trace_id.is_some());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn mcp_call_history_marks_omitted_session_as_absent() {
        let directory = test_directory();
        record_mcp_call(
            &directory,
            "claude",
            "search",
            None,
            &serde_json::json!({"query":"latest CLI"}),
            8,
            false,
            serde_json::json!({"results": []}),
        )
        .unwrap();
        let history =
            fs::read_to_string(directory.join(".membrick/mcp-call-history.jsonl")).unwrap();
        let record: McpCallRecord = serde_json::from_str(history.trim()).unwrap();
        assert!(record.trace_id.is_some());
        assert!(record.session_id.is_none());
        assert_eq!(record.session_attribution, "absent");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn existing_unlocked_telemetry_lock_file_does_not_block_append() {
        let directory = test_directory();
        let membrick = directory.join(".membrick");
        fs::create_dir_all(&membrick).unwrap();
        fs::write(membrick.join("mcp-call-history.lock"), b"stale lock marker").unwrap();

        record_mcp_call(
            &directory,
            "claude",
            "search",
            None,
            &serde_json::json!({"query":"recover from stale marker"}),
            1,
            false,
            serde_json::json!({"results": []}),
        )
        .expect("an unlocked lock file left by a crashed process must be reusable");

        let history = fs::read_to_string(membrick.join(MCP_CALL_HISTORY_FILE)).unwrap();
        assert_eq!(history.lines().count(), 1);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn mcp_call_history_accepts_legacy_records_without_attribution_fields() {
        let record: McpCallRecord = serde_json::from_value(serde_json::json!({
            "timestamp_ms": 1,
            "provider": "codex",
            "tool": "search",
            "arguments": {"query":"q"},
            "duration_ms": 1,
            "error": false,
            "response": {}
        }))
        .unwrap();
        assert!(record.trace_id.is_none());
        assert!(record.session_id.is_none());
        assert_eq!(record.session_attribution, "absent");
    }

    #[test]
    fn empty_snapshot_has_zero_rates() {
        let snapshot = OperationalTelemetry::new().snapshot();
        assert_eq!(snapshot.query_summary.requests, 0);
        assert_eq!(snapshot.query_summary.requests_per_second, 0.0);
    }

    #[test]
    fn records_query_counts_and_latency_buckets() {
        let telemetry = OperationalTelemetry::new();
        telemetry.record_query(8, false, true);
        telemetry.record_query(100, true, false);
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let snapshot = loop {
            let snapshot = telemetry.snapshot();
            if snapshot.query_summary.requests == 2 || std::time::Instant::now() >= deadline {
                break snapshot;
            }
            thread::yield_now();
        };
        assert_eq!(snapshot.query_summary.requests, 2);
        assert_eq!(snapshot.query_summary.errors, 1);
        assert_eq!(snapshot.query_summary.empty_results, 1);
        assert!(snapshot.query_summary.p50_ms > 0.0);
        assert!(!snapshot.query_series.is_empty());
    }

    #[test]
    fn reveals_bug_record_query_waits_for_the_telemetry_mutex() {
        let telemetry = OperationalTelemetry::new();
        let guard = telemetry.state.lock().unwrap();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker_telemetry = telemetry.clone();
        thread::spawn(move || {
            worker_telemetry.record_query(8, false, false);
            done_tx.send(()).unwrap();
        });

        assert!(
            done_rx.recv_timeout(Duration::from_millis(50)).is_ok(),
            "record_query blocked behind the telemetry mutex"
        );
        drop(guard);
    }

    #[test]
    fn provider_lifecycle_ledger_is_bounded_and_opaque() {
        let root = std::env::temp_dir().join(format!("membrick-provider-telemetry-{}", now_ms()));
        record_provider_lifecycle_event(
            &root,
            "codex",
            "private-session-id",
            "SessionStart",
            &serde_json::json!({}),
        )
        .unwrap();
        record_provider_lifecycle_event(
            &root,
            "codex",
            "private-session-id",
            "PreToolUse",
            &serde_json::json!({
                "tool_name": "read_file",
                "tool_input": {"path": "README.md", "api_key": "do-not-store"}
            }),
        )
        .unwrap();
        record_provider_lifecycle_event(
            &root,
            "codex",
            "private-session-id",
            "UserPromptSubmit",
            &serde_json::json!({"prompt": "show the current index status"}),
        )
        .unwrap();
        record_provider_lifecycle_event(
            &root,
            "codex",
            "private-session-id",
            "PostToolUse",
            &serde_json::json!({
                "tool_name": "read_file",
                "tool_response": {"content": "index is ready", "token": "sk-live-secret"}
            }),
        )
        .unwrap();
        record_provider_lifecycle_event(
            &root,
            "codex",
            "private-session-id",
            "Stop",
            &serde_json::json!({"final_response": "The index is ready.", "reason": "completed"}),
        )
        .unwrap();
        let status = provider_lifecycle_status(&root, "codex").unwrap().unwrap();
        assert_eq!(status.events_total, 5);
        assert_eq!(status.sessions_started, 1);
        assert_eq!(status.retrieval_events, 2);
        assert_eq!(status.events.len(), 5);
        assert_eq!(status.events[1].tool_name.as_deref(), Some("read_file"));
        assert!(status.events[1]
            .tool_arguments
            .as_deref()
            .unwrap()
            .contains("REDACTED"));
        assert_eq!(
            status.events[2].prompt_preview.as_deref(),
            Some("show the current index status")
        );
        assert!(status.events[3]
            .tool_response_preview
            .as_deref()
            .unwrap()
            .contains("REDACTED"));
        assert_eq!(
            status.events[4].stop_response_preview.as_deref(),
            Some("The index is ready.")
        );
        assert_ne!(status.events[0].session_key, "private-session-id");
        assert!(!serde_json::to_string(&status)
            .unwrap()
            .contains("private-session-id"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn provider_counters_survive_event_window_eviction() {
        let root = test_directory().join(format!("provider-counters-{}", now_ms()));
        let record = |session: &str, event: &str, payload: serde_json::Value| {
            record_provider_lifecycle_event(&root, "codex", session, event, &payload).unwrap();
        };
        record(
            "session-a",
            "UserPromptSubmit",
            serde_json::json!({"prompt": "build it"}),
        );
        record(
            "session-a",
            "PreToolUse",
            serde_json::json!({"tool_name": "shell", "tool_input": {"cmd": "ls"}}),
        );
        record(
            "session-a",
            "PostToolUse",
            serde_json::json!({"tool_name": "shell", "tool_response": "ok"}),
        );
        record(
            "session-a",
            "SubagentStart",
            serde_json::json!({"agent_id": "a1", "agent_type": "explorer"}),
        );
        record(
            "session-a",
            "SubagentStop",
            serde_json::json!({"agent_id": "a1", "agent_type": "explorer"}),
        );
        record(
            "session-a",
            "TurnUsage",
            serde_json::json!({"usage": {
                "input_tokens": 100,
                "output_tokens": 20,
                "cache_creation_input_tokens": 30,
                "cache_read_input_tokens": 400,
                "total_tokens": 120
            }}),
        );
        record(
            "session-a",
            "Stop",
            serde_json::json!({"final_response": "done"}),
        );
        // A stop with no open prompt counts as a stop but not as a cycle.
        record("session-b", "Stop", serde_json::json!({}));
        for index in 0..600 {
            record(
                "session-a",
                "MemoryRetrieved",
                serde_json::json!({"query": format!("q{index}"), "retrieved_memory_count": 1}),
            );
        }

        let status = provider_lifecycle_status(&root, "codex").unwrap().unwrap();
        assert_eq!(status.events.len(), MAX_PROVIDER_TELEMETRY_EVENTS);
        assert!(status
            .events
            .iter()
            .all(|event| event.event == "MemoryRetrieved"));
        let counters = &status.counters;
        assert!(counters.since_ms > 0);
        assert_eq!(counters.prompts, 1);
        assert_eq!(counters.stops, 2);
        assert_eq!(counters.tool_calls, 1);
        assert_eq!(counters.tool_results, 1);
        assert_eq!(counters.subagent_starts, 1);
        assert_eq!(counters.subagent_stops, 1);
        assert_eq!(counters.usage_events, 1);
        assert_eq!(counters.input_tokens, 100);
        assert_eq!(counters.output_tokens, 20);
        assert_eq!(counters.cache_creation_input_tokens, 30);
        assert_eq!(counters.cache_read_input_tokens, 400);
        assert_eq!(counters.reported_total_tokens, 120);
        assert_eq!(counters.cycles_completed, 1);
        assert_eq!(
            counters.cycle_duration_buckets.iter().sum::<u64>(),
            counters.cycles_completed
        );
        assert!(counters.cycle_duration_quantile_ms(0.5).is_some());
        assert!(counters.open_prompts.is_empty());
        let milestones = status
            .milestone_events
            .iter()
            .map(|event| event.event.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            milestones,
            [
                "UserPromptSubmit",
                "SubagentStart",
                "SubagentStop",
                "TurnUsage",
                "Stop",
                "Stop"
            ]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn session_end_drops_an_open_prompt_without_counting_a_cycle() {
        let root = test_directory().join(format!("provider-session-end-{}", now_ms()));
        let record = |session: &str, event: &str| {
            record_provider_lifecycle_event(&root, "codex", session, event, &serde_json::json!({}))
                .unwrap();
        };
        record("session-a", "UserPromptSubmit");
        record("session-a", "SessionEnd");
        // A stop after the session ended has no open prompt to close.
        record("session-a", "Stop");

        let status = provider_lifecycle_status(&root, "codex").unwrap().unwrap();
        let counters = &status.counters;
        assert_eq!(counters.prompts, 1);
        assert_eq!(counters.stops, 1);
        assert_eq!(counters.cycles_completed, 0);
        assert_eq!(counters.cycle_duration_sum_ms, 0);
        assert_eq!(counters.cycle_duration_buckets.iter().sum::<u64>(), 0);
        assert!(counters.cycle_duration_quantile_ms(0.5).is_none());
        assert!(counters.open_prompts.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn subagent_counters_track_open_interrupted_and_ended_runs() {
        let root = test_directory().join(format!("provider-subagents-{}", now_ms()));
        let record = |session: &str, event: &str, agent: &str| {
            record_provider_lifecycle_event(
                &root,
                "claude",
                session,
                event,
                &serde_json::json!({"agent_id": agent}),
            )
            .unwrap();
        };
        // A run killed by an API error and resumed sends two starts, one stop.
        record("session-a", "SubagentStart", "resumed");
        record("session-a", "SubagentStart", "resumed");
        record("session-a", "SubagentStop", "resumed");
        // A run whose session ends without a stop.
        record("session-b", "SubagentStart", "orphan");
        record("session-b", "PreToolUse", "orphan");
        record("session-b", "SessionEnd", "");
        // A run still going in another session.
        record("session-c", "SubagentStart", "live");
        record("session-c", "PostToolUse", "live");
        // A stop that lands just after its session ended counts once, as a stop.
        record("session-d", "SubagentStart", "late");
        record("session-d", "SessionEnd", "");
        record("session-d", "SubagentStop", "late");

        let status = provider_lifecycle_status(&root, "claude").unwrap().unwrap();
        let counters = &status.counters;
        assert_eq!(counters.subagent_starts, 5);
        assert_eq!(counters.subagent_stops, 2);
        assert_eq!(counters.subagent_interrupted, 1);
        assert_eq!(counters.subagent_ended_without_stop, 1);
        assert_eq!(counters.open_subagents.keys().collect::<Vec<_>>(), ["live"]);
        let live = &counters.open_subagents["live"];
        assert!(live.last_seen_ms >= live.started_ms);
        assert_eq!(
            counters.subagent_starts,
            counters.subagent_stops
                + counters.subagent_interrupted
                + counters.subagent_ended_without_stop
                + counters.open_subagents.len() as u64
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn provider_ledger_without_counters_still_loads() {
        let legacy = serde_json::json!({
            "schema_version": 1,
            "provider": "codex",
            "last_event": "Stop",
            "last_seen_ms": 10,
            "events_total": 3,
            "sessions_started": 1,
            "sessions_ended": 0,
            "sessions_active": 1,
            "retrieval_events": 1,
            "capture_events": 1,
            "events": []
        });
        let status: ProviderLifecycleStatus = serde_json::from_value(legacy).unwrap();
        assert_eq!(status.events_total, 3);
        assert_eq!(status.counters.since_ms, 0);
        assert_eq!(status.counters.prompts, 0);
        assert!(status.milestone_events.is_empty());
        assert_eq!(status.counters.cycle_duration_quantile_ms(0.5), None);
    }
}
