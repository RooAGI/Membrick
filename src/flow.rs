//! Provider-neutral flow definitions and orchestration event contracts.
//!
//! Flow templates describe lifecycle shape and validation rules. A FlowRun is
//! an instance associated with an existing board; its append-only events and
//! projection live in the shared MemoryService document store.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Digest;
use std::collections::BTreeMap;

pub const FLOW_RUN_DOC_KIND: &str = "flow_run";
pub const FLOW_EVENT_DOC_KIND: &str = "flow_event";
pub const FLOW_ID_FILTER: &str = "flow_id";
pub const FLOW_OWNER_FILTER: &str = "flow_owner";
pub const FLOW_WORKSPACE_FILTER: &str = "flow_workspace";
pub const FLOW_EVENT_REVISION_FILTER: &str = "flow_revision";
pub const FLOW_EVENT_ID_FILTER: &str = "flow_event_id";
pub const FLOW_REQUEST_ID_FILTER: &str = "flow_request_id";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum FlowTaskState {
    Ready,
    Assigned,
    Running,
    Blocked,
    Review,
    ChangesRequested,
    Done,
    Cancelled,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FlowState {
    Active,
    Completed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FlowReferenceKind {
    Board,
    Post,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FlowReference {
    pub kind: FlowReferenceKind,
    pub id: String,
    pub relation: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FlowStage {
    pub id: String,
    pub name: String,
    pub transitions_to: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FlowTransition {
    pub event: FlowEventKind,
    pub from_stage: Option<String>,
    pub to_stage: Option<String>,
    pub from_task_state: Option<FlowTaskState>,
    pub to_task_state: Option<FlowTaskState>,
    pub requires_coordinator: bool,
    pub requires_reviewer: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FlowRetryPolicy {
    pub max_attempts: u32,
    pub retry_states: Vec<FlowTaskState>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FlowTemplate {
    #[serde(rename = "template_id", alias = "id")]
    pub id: String,
    pub version: u32,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub content_hash: String,
    pub board_template: FlowBoardTemplateRef,
    pub stages: Vec<FlowStage>,
    pub task_states: Vec<FlowTaskState>,
    pub transitions: Vec<FlowTransition>,
    pub requires_review: bool,
    pub coordinator_policy: String,
    pub retry_policy: FlowRetryPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FlowBoardTemplateRef {
    pub template_id: String,
    pub version: u32,
    pub content_hash: String,
}

/// Uniform reference envelope used when an AgentFlow definition is pinned to a run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FlowTemplateInstance {
    pub issuer: String,
    pub template_id: String,
    pub version: u32,
    pub content_hash: String,
    pub snapshot: FlowTemplate,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FlowEventKind {
    FlowStarted,
    StageChanged,
    CoordinatorClaimed,
    CoordinatorHandedOff,
    CoordinatorReleased,
    TaskCreated,
    TaskAssigned,
    TaskStarted,
    Progress,
    Blocked,
    ReadyForReview,
    ReviewApproved,
    ReviewChangesRequested,
    TaskCancelled,
    FlowCompleted,
    FlowCancelled,
    ActionRequested,
    ActionConsumed,
    ReferenceAdded,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FlowCoordinator {
    pub agent_id: String,
    pub provider: String,
    pub fencing_token: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FlowTask {
    pub task_id: String,
    pub title: String,
    pub state: FlowTaskState,
    pub assignee: Option<FlowCoordinator>,
    pub attempt_id: Option<String>,
    pub attempts: u32,
    pub summary: Option<String>,
    pub blocker: Option<String>,
    pub evidence: Vec<String>,
    #[serde(default)]
    pub post_ids: Vec<String>,
    pub revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FlowActionState {
    Pending,
    Consumed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FlowActionIntent {
    pub action_id: String,
    pub action: String,
    pub payload: Value,
    pub requested_by: String,
    pub event_id: String,
    pub coordinator_fence: u64,
    pub state: FlowActionState,
    pub consumed_by: Option<String>,
    pub consumed_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FlowRun {
    pub flow_id: String,
    pub owner: String,
    pub workspace: String,
    pub board_id: String,
    pub template_id: String,
    pub template_version: u32,
    /// Exact AgentFlow-owned definition pinned at creation. Missing only on legacy stored runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_template_instance: Option<FlowTemplateInstance>,
    pub goal: String,
    pub references: Vec<FlowReference>,
    pub stage: String,
    pub state: FlowState,
    pub revision: u64,
    pub coordinator: Option<FlowCoordinator>,
    pub coordinator_fence: u64,
    pub tasks: BTreeMap<String, FlowTask>,
    pub pending_actions: Vec<FlowActionIntent>,
    pub created_by: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FlowEvent {
    pub event_id: String,
    pub request_id: String,
    pub flow_id: String,
    pub revision: u64,
    pub actor: String,
    pub provider: String,
    pub attempt_id: Option<String>,
    pub kind: FlowEventKind,
    pub task_id: Option<String>,
    #[serde(default)]
    pub post_id: Option<String>,
    pub data: Value,
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FlowSnapshot {
    pub run: FlowRun,
    pub events: Vec<FlowEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowEventAppend {
    pub owner: String,
    pub workspace: String,
    pub flow_id: String,
    pub expected_revision: u64,
    #[serde(default)]
    pub coordinator_fence: Option<u64>,
    pub event_id: String,
    pub request_id: String,
    pub actor: String,
    pub provider: String,
    #[serde(default)]
    pub attempt_id: Option<String>,
    pub kind: FlowEventKind,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub post_id: Option<String>,
    #[serde(default)]
    pub data: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowCreateRequest {
    pub owner: String,
    pub workspace: String,
    pub board_id: String,
    pub flow_template_instance: FlowTemplateInstance,
    pub goal: String,
    #[serde(default)]
    pub references: Vec<FlowReference>,
    pub created_by: String,
    pub request_id: String,
}

/// The default provider-neutral lifecycle contract. Hosts can map its events
/// to provider-native flows while Membrick stores shared coordination state.
fn builtin_templates() -> Vec<FlowTemplate> {
    use FlowEventKind as E;
    use FlowTaskState as T;
    let states = vec![
        T::Ready,
        T::Assigned,
        T::Running,
        T::Blocked,
        T::Review,
        T::ChangesRequested,
        T::Done,
        T::Cancelled,
    ];
    let stages = vec![
        FlowStage {
            id: "planning".into(),
            name: "Planning".into(),
            transitions_to: vec!["execution".into()],
        },
        FlowStage {
            id: "execution".into(),
            name: "Execution".into(),
            transitions_to: vec!["review".into()],
        },
        FlowStage {
            id: "review".into(),
            name: "Review".into(),
            transitions_to: vec!["execution".into(), "complete".into()],
        },
        FlowStage {
            id: "complete".into(),
            name: "Complete".into(),
            transitions_to: vec![],
        },
        FlowStage {
            id: "cancelled".into(),
            name: "Cancelled".into(),
            transitions_to: vec![],
        },
    ];
    let mut transitions = vec![
        FlowTransition {
            event: E::StageChanged,
            from_stage: Some("planning".into()),
            to_stage: Some("execution".into()),
            from_task_state: None,
            to_task_state: None,
            requires_coordinator: true,
            requires_reviewer: false,
        },
        FlowTransition {
            event: E::StageChanged,
            from_stage: Some("execution".into()),
            to_stage: Some("review".into()),
            from_task_state: None,
            to_task_state: None,
            requires_coordinator: true,
            requires_reviewer: false,
        },
        FlowTransition {
            event: E::StageChanged,
            from_stage: Some("review".into()),
            to_stage: Some("execution".into()),
            from_task_state: None,
            to_task_state: None,
            requires_coordinator: true,
            requires_reviewer: false,
        },
        FlowTransition {
            event: E::FlowCompleted,
            from_stage: Some("review".into()),
            to_stage: Some("complete".into()),
            from_task_state: None,
            to_task_state: None,
            requires_coordinator: true,
            requires_reviewer: false,
        },
    ];
    for (event, from, to, coord, reviewer) in [
        (E::TaskCreated, None, Some(T::Ready), true, false),
        (
            E::TaskAssigned,
            Some(T::Ready),
            Some(T::Assigned),
            true,
            false,
        ),
        (
            E::TaskAssigned,
            Some(T::Blocked),
            Some(T::Assigned),
            true,
            false,
        ),
        (
            E::TaskAssigned,
            Some(T::ChangesRequested),
            Some(T::Assigned),
            true,
            false,
        ),
        (
            E::TaskStarted,
            Some(T::Assigned),
            Some(T::Running),
            false,
            false,
        ),
        (
            E::Progress,
            Some(T::Running),
            Some(T::Running),
            false,
            false,
        ),
        (E::Blocked, Some(T::Running), Some(T::Blocked), false, false),
        (
            E::ReadyForReview,
            Some(T::Running),
            Some(T::Review),
            false,
            false,
        ),
        (
            E::ReviewApproved,
            Some(T::Review),
            Some(T::Done),
            false,
            true,
        ),
        (
            E::ReviewChangesRequested,
            Some(T::Review),
            Some(T::ChangesRequested),
            false,
            true,
        ),
        (
            E::TaskCancelled,
            Some(T::Ready),
            Some(T::Cancelled),
            true,
            false,
        ),
        (
            E::TaskCancelled,
            Some(T::Assigned),
            Some(T::Cancelled),
            true,
            false,
        ),
        (
            E::TaskCancelled,
            Some(T::Running),
            Some(T::Cancelled),
            true,
            false,
        ),
        (
            E::TaskCancelled,
            Some(T::Blocked),
            Some(T::Cancelled),
            true,
            false,
        ),
        (
            E::TaskCancelled,
            Some(T::ChangesRequested),
            Some(T::Cancelled),
            true,
            false,
        ),
    ] {
        transitions.push(FlowTransition {
            event,
            from_stage: None,
            to_stage: None,
            from_task_state: from,
            to_task_state: to,
            requires_coordinator: coord,
            requires_reviewer: reviewer,
        });
    }
    vec![FlowTemplate {
        id: "agent_work".into(),
        version: 1,
        name: "Agent work with independent review".into(),
        description: Some("A provider-neutral plan, task execution, progress, review, and handoff lifecycle.".into()),
        content_hash: "".into(),
        board_template: FlowBoardTemplateRef { template_id: "project".into(), version: 1, content_hash: "".into() },
        stages,
        task_states: states,
        transitions,
        requires_review: true,
        coordinator_policy: "Any connected agent may coordinate. One coordinator is active at a time; claim and handoff events advance a fencing token.".into(),
        retry_policy: FlowRetryPolicy {
            max_attempts: 5,
            retry_states: vec![T::Blocked, T::ChangesRequested],
        },
    }]
}

pub(crate) fn legacy_flow_template(
    template_id: &str,
    version: Option<u32>,
) -> Option<FlowTemplate> {
    builtin_templates().into_iter().find(|template| {
        template.id == template_id && version.map_or(true, |value| value == template.version)
    })
}

pub fn validate_flow_template_instance(instance: &FlowTemplateInstance) -> anyhow::Result<()> {
    if instance.issuer.trim().is_empty() || instance.issuer.len() > 128 {
        anyhow::bail!("FlowTemplate issuer must be 1-128 bytes");
    }
    validate_flow_template(&instance.snapshot)?;
    anyhow::ensure!(
        instance.template_id == instance.snapshot.id
            && instance.version == instance.snapshot.version
            && instance.content_hash == instance.snapshot.content_hash,
        "FlowTemplate instance envelope does not match its snapshot identity/hash"
    );
    Ok(())
}

/// Validate an AgentFlow supplied FlowTemplate snapshot and its JCS content hash.
pub fn validate_flow_template(template: &FlowTemplate) -> anyhow::Result<()> {
    use sha2::Digest;
    if template.id.trim().is_empty() || template.id.len() > 128 || template.version == 0 {
        anyhow::bail!("FlowTemplate identity must be valid");
    }
    if template.name.trim().is_empty()
        || template.name.len() > 256
        || template
            .description
            .as_ref()
            .is_some_and(|value| value.len() > 2048)
        || template.board_template.template_id.trim().is_empty()
        || template.board_template.template_id.len() > 128
        || template.board_template.version == 0
    {
        anyhow::bail!("FlowTemplate metadata or board template reference is invalid");
    }
    if template.stages.is_empty()
        || template.stages.len() > 64
        || template.task_states.is_empty()
        || template.task_states.len() > 32
        || template.transitions.is_empty()
        || template.transitions.len() > 256
        || template.retry_policy.max_attempts > 100
        || template.retry_policy.retry_states.len() > 32
    {
        anyhow::bail!("FlowTemplate lifecycle definition exceeds supported bounds");
    }
    let template_bytes = serde_jcs::to_vec(&serde_json::to_value(template)?)?;
    if template_bytes.len() > 64 * 1024 {
        anyhow::bail!("FlowTemplate snapshot exceeds 65536 bytes");
    }
    let mut stage_ids = std::collections::HashSet::new();
    for stage in &template.stages {
        if stage.id.trim().is_empty() || stage.id.len() > 128 || !stage_ids.insert(&stage.id) {
            anyhow::bail!("FlowTemplate stage IDs must be unique and 1-128 bytes");
        }
        if stage.name.trim().is_empty() || stage.name.len() > 256 {
            anyhow::bail!("FlowTemplate stage names must be 1-256 bytes");
        }
    }
    let mut task_states = std::collections::HashSet::new();
    for state in &template.task_states {
        if !task_states.insert(*state) {
            anyhow::bail!("FlowTemplate task states must be unique");
        }
    }
    for transition in &template.transitions {
        if transition
            .from_stage
            .as_ref()
            .is_some_and(|value| !stage_ids.contains(value))
            || transition
                .to_stage
                .as_ref()
                .is_some_and(|value| !stage_ids.contains(value))
        {
            anyhow::bail!("FlowTemplate transition references an unknown stage");
        }
        if transition
            .from_task_state
            .as_ref()
            .is_some_and(|value| !task_states.contains(value))
            || transition
                .to_task_state
                .as_ref()
                .is_some_and(|value| !task_states.contains(value))
        {
            anyhow::bail!("FlowTemplate transition references an undeclared task state");
        }
    }
    let mut definition = serde_json::to_value(template)?;
    definition
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("FlowTemplate must serialize as an object"))?
        .remove("content_hash");
    let canonical = serde_jcs::to_vec(&definition)?;
    let expected = format!("sha256:{:x}", sha2::Sha256::digest(canonical));
    if template.content_hash != expected {
        anyhow::bail!("FlowTemplate content_hash mismatch; expected {expected}");
    }
    Ok(())
}

pub fn flow_run_doc_id(flow_id: &str) -> String {
    format!("membrick:flow:{flow_id}:run")
}

pub fn flow_event_doc_id(flow_id: &str, event_id: &str) -> String {
    let digest = sha2::Sha256::digest(event_id.as_bytes());
    let encoded = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("membrick:flow:{flow_id}:event:{encoded}")
}

pub fn flow_run_filters(run: &FlowRun) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("doc_kind".into(), FLOW_RUN_DOC_KIND.into()),
        (FLOW_ID_FILTER.into(), run.flow_id.clone()),
        (FLOW_OWNER_FILTER.into(), run.owner.clone()),
        (FLOW_WORKSPACE_FILTER.into(), run.workspace.clone()),
    ])
}

pub fn flow_event_filters(
    event: &FlowEvent,
    owner: &str,
    workspace: &str,
) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("doc_kind".into(), FLOW_EVENT_DOC_KIND.into()),
        (FLOW_ID_FILTER.into(), event.flow_id.clone()),
        (FLOW_OWNER_FILTER.into(), owner.into()),
        (FLOW_WORKSPACE_FILTER.into(), workspace.into()),
        (FLOW_EVENT_ID_FILTER.into(), event.event_id.clone()),
        (FLOW_REQUEST_ID_FILTER.into(), event.request_id.clone()),
        (
            FLOW_EVENT_REVISION_FILTER.into(),
            event.revision.to_string(),
        ),
    ])
}

impl FlowRun {
    pub fn new(
        flow_id: String,
        owner: String,
        workspace: String,
        board_id: String,
        template_instance: &FlowTemplateInstance,
        goal: String,
        references: Vec<FlowReference>,
        created_by: String,
        timestamp: String,
    ) -> Self {
        Self {
            flow_id,
            owner,
            workspace,
            board_id,
            template_id: template_instance.template_id.clone(),
            template_version: template_instance.version,
            flow_template_instance: Some(template_instance.clone()),
            goal,
            references,
            stage: "planning".into(),
            state: FlowState::Active,
            revision: 0,
            coordinator: None,
            coordinator_fence: 0,
            tasks: BTreeMap::new(),
            pending_actions: Vec::new(),
            created_by,
            created_at: timestamp.clone(),
            updated_at: timestamp,
        }
    }
}
