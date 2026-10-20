//! Durable FlowTemplate/FlowRun lifecycle operations.

use super::*;
use crate::flow::*;
use chrono::{SecondsFormat, Utc};
use serde_json::{json, Value};
use sha2::Digest;
use std::collections::BTreeMap;

pub const FLOW_CREATE_REQUEST_FILTER: &str = "flow_create_request_id";

fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn json_document(
    doc_id: String,
    source: &str,
    concept: String,
    content: &str,
    timestamp: &str,
    author: Option<&str>,
    filters: BTreeMap<String, String>,
    group_id: Option<String>,
) -> crate::SourceDocument {
    crate::SourceDocument {
        doc_id,
        source: source.to_string(),
        content: content.into(),
        concept,
        group_id,
        headings: Vec::new(),
        links: Vec::new(),
        timestamp: Some(timestamp.to_string()),
        doc_length: content.len(),
        author_agent: author.map(str::to_string),
        filters,
        key_phrases: Vec::new(),
        key_phrase_extraction_hash: String::new(),
    }
}

fn digest(parts: &[&str]) -> String {
    let mut hasher = sha2::Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn validate_identifier(value: &str, label: &str, max: usize) -> anyhow::Result<()> {
    anyhow::ensure!(!value.trim().is_empty(), "{label} must not be empty");
    anyhow::ensure!(value.len() <= max, "{label} exceeds {max} bytes");
    Ok(())
}

fn parse_run(document: &crate::SourceDocument) -> anyhow::Result<FlowRun> {
    serde_json::from_str(document.content.as_ref())
        .map_err(|error| anyhow::anyhow!("invalid stored FlowRun {}: {error}", document.doc_id))
}

fn parse_event(document: &crate::SourceDocument) -> anyhow::Result<FlowEvent> {
    serde_json::from_str(document.content.as_ref())
        .map_err(|error| anyhow::anyhow!("invalid stored FlowEvent {}: {error}", document.doc_id))
}

fn actor_is_coordinator(run: &FlowRun, actor: &str, fence: Option<u64>) -> anyhow::Result<()> {
    let coordinator = run
        .coordinator
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("this event requires an active coordinator"))?;
    anyhow::ensure!(
        coordinator.agent_id == actor,
        "coordinator ownership conflict"
    );
    anyhow::ensure!(
        fence == Some(run.coordinator_fence),
        "stale coordinator fencing token; current token is {}",
        run.coordinator_fence
    );
    Ok(())
}

fn cancel_pending_actions(run: &mut FlowRun) {
    for action in &mut run.pending_actions {
        if action.state == FlowActionState::Pending {
            action.state = FlowActionState::Cancelled;
        }
    }
}

fn required_string(data: &Value, field: &str) -> anyhow::Result<String> {
    data.get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("event data requires non-empty {field:?}"))
}

fn optional_string(data: &Value, field: &str) -> Option<String> {
    data.get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn template_transition<'a>(
    template: &'a FlowTemplate,
    event: &FlowEvent,
    from_stage: Option<&str>,
    to_stage: Option<&str>,
    from_state: Option<&FlowTaskState>,
    to_state: Option<&FlowTaskState>,
) -> anyhow::Result<&'a FlowTransition> {
    let transition = template
        .transitions
        .iter()
        .find(|transition| {
            transition.event == event.kind
                && transition.from_stage.as_deref() == from_stage
                && transition.to_stage.as_deref() == to_stage
                && transition.from_task_state.as_ref() == from_state
                && transition.to_task_state.as_ref() == to_state
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "event transition is not allowed by FlowTemplate {}@{}",
                template.id,
                template.version
            )
        })?;
    Ok(transition)
}

fn apply_event(
    run: &mut FlowRun,
    template: &FlowTemplate,
    event: &FlowEvent,
) -> anyhow::Result<()> {
    run.revision = event.revision;
    run.updated_at = event.timestamp.clone();
    if run.state != FlowState::Active {
        anyhow::bail!("flow is {:?} and cannot accept {:?}", run.state, event.kind);
    }
    match event.kind {
        FlowEventKind::FlowStarted => {}
        FlowEventKind::CoordinatorClaimed => {
            anyhow::ensure!(
                run.coordinator.is_none(),
                "flow already has an active coordinator"
            );
            run.coordinator_fence = run.coordinator_fence.saturating_add(1);
            run.coordinator = Some(FlowCoordinator {
                agent_id: event.actor.clone(),
                provider: event.provider.clone(),
                fencing_token: run.coordinator_fence,
            });
        }
        FlowEventKind::CoordinatorHandedOff => {
            actor_is_coordinator(
                run,
                &event.actor,
                event.data.get("coordinator_fence").and_then(Value::as_u64),
            )?;
            let agent_id = required_string(&event.data, "next_coordinator")?;
            let provider = required_string(&event.data, "next_provider")?;
            anyhow::ensure!(
                agent_id != event.actor,
                "handoff target must differ from current coordinator"
            );
            cancel_pending_actions(run);
            run.coordinator_fence = run.coordinator_fence.saturating_add(1);
            run.coordinator = Some(FlowCoordinator {
                agent_id,
                provider,
                fencing_token: run.coordinator_fence,
            });
        }
        FlowEventKind::CoordinatorReleased => {
            actor_is_coordinator(
                run,
                &event.actor,
                event.data.get("coordinator_fence").and_then(Value::as_u64),
            )?;
            cancel_pending_actions(run);
            run.coordinator_fence = run.coordinator_fence.saturating_add(1);
            run.coordinator = None;
        }
        FlowEventKind::StageChanged => {
            let next = required_string(&event.data, "stage")?;
            anyhow::ensure!(
                template.stages.iter().any(|stage| stage.id == next),
                "unknown stage {next:?}"
            );
            anyhow::ensure!(
                next != "complete" && next != "cancelled",
                "terminal stages require flow_completed or flow_cancelled events"
            );
            let transition =
                template_transition(template, event, Some(&run.stage), Some(&next), None, None)?;
            if transition.requires_coordinator {
                actor_is_coordinator(
                    run,
                    &event.actor,
                    event.data.get("coordinator_fence").and_then(Value::as_u64),
                )?;
            }
            run.stage = next;
        }
        FlowEventKind::ReferenceAdded => {
            actor_is_coordinator(
                run,
                &event.actor,
                event.data.get("coordinator_fence").and_then(Value::as_u64),
            )?;
            let reference: FlowReference =
                serde_json::from_value(
                    event.data.get("reference").cloned().ok_or_else(|| {
                        anyhow::anyhow!("reference_added requires data.reference")
                    })?,
                )?;
            anyhow::ensure!(
                !run.references.iter().any(|existing| existing == &reference),
                "FlowRun reference already exists"
            );
            run.references.push(reference);
        }
        FlowEventKind::TaskCreated => {
            let task_id = event
                .task_id
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("task_created requires task_id"))?;
            anyhow::ensure!(
                !run.tasks.contains_key(task_id),
                "task_id already exists: {task_id}"
            );
            let title = required_string(&event.data, "title")?;
            let transition = template_transition(
                template,
                event,
                None,
                None,
                None,
                Some(&FlowTaskState::Ready),
            )?;
            if transition.requires_coordinator {
                actor_is_coordinator(
                    run,
                    &event.actor,
                    event.data.get("coordinator_fence").and_then(Value::as_u64),
                )?;
            }
            run.tasks.insert(
                task_id.to_string(),
                FlowTask {
                    task_id: task_id.to_string(),
                    title,
                    state: FlowTaskState::Ready,
                    assignee: None,
                    attempt_id: None,
                    attempts: 0,
                    summary: optional_string(&event.data, "summary"),
                    blocker: None,
                    evidence: Vec::new(),
                    post_ids: event.post_id.iter().cloned().collect(),
                    revision: event.revision,
                },
            );
        }
        FlowEventKind::TaskAssigned
        | FlowEventKind::TaskStarted
        | FlowEventKind::Progress
        | FlowEventKind::Blocked
        | FlowEventKind::ReadyForReview
        | FlowEventKind::ReviewApproved
        | FlowEventKind::ReviewChangesRequested
        | FlowEventKind::TaskCancelled => {
            let task_id = event
                .task_id
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("task event requires task_id"))?;
            let task = run
                .tasks
                .get(task_id)
                .ok_or_else(|| anyhow::anyhow!("unknown task_id: {task_id}"))?;
            let post_id = event
                .post_id
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("task lifecycle events require post_id"))?;
            let old_state = task.state.clone();
            let new_state = match event.kind {
                FlowEventKind::TaskAssigned => FlowTaskState::Assigned,
                FlowEventKind::TaskStarted => FlowTaskState::Running,
                FlowEventKind::Progress => FlowTaskState::Running,
                FlowEventKind::Blocked => FlowTaskState::Blocked,
                FlowEventKind::ReadyForReview => FlowTaskState::Review,
                FlowEventKind::ReviewApproved => FlowTaskState::Done,
                FlowEventKind::ReviewChangesRequested => FlowTaskState::ChangesRequested,
                FlowEventKind::TaskCancelled => FlowTaskState::Cancelled,
                _ => unreachable!(),
            };
            let transition = template_transition(
                template,
                event,
                None,
                None,
                Some(&old_state),
                Some(&new_state),
            )?;
            if transition.requires_coordinator {
                actor_is_coordinator(
                    run,
                    &event.actor,
                    event.data.get("coordinator_fence").and_then(Value::as_u64),
                )?;
            } else if transition.requires_reviewer {
                anyhow::ensure!(
                    task.assignee
                        .as_ref()
                        .is_some_and(|assignee| assignee.agent_id != event.actor),
                    "review must be authored by an agent other than the assignee"
                );
            } else {
                anyhow::ensure!(
                    task.assignee
                        .as_ref()
                        .is_some_and(|assignee| assignee.agent_id == event.actor),
                    "task update actor is not the assigned worker"
                );
                let attempt_id = event
                    .attempt_id
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("task report requires attempt_id"))?;
                anyhow::ensure!(
                    task.attempt_id.as_deref() == Some(attempt_id),
                    "task report attempt_id is stale"
                );
            }
            let task = run
                .tasks
                .get_mut(task_id)
                .expect("task existence was validated");
            if !task.post_ids.iter().any(|existing| existing == post_id) {
                task.post_ids.push(post_id.to_string());
            }
            if matches!(event.kind, FlowEventKind::TaskAssigned) {
                let agent_id = required_string(&event.data, "assignee")?;
                let provider = required_string(&event.data, "provider")?;
                if task.state != FlowTaskState::Ready {
                    anyhow::ensure!(
                        template.retry_policy.retry_states.contains(&task.state),
                        "task state {:?} is not retryable by this FlowTemplate",
                        task.state
                    );
                    task.attempts = task.attempts.saturating_add(1);
                    anyhow::ensure!(
                        task.attempts <= template.retry_policy.max_attempts,
                        "task exceeded max attempts"
                    );
                } else {
                    task.attempts = 1;
                }
                task.assignee = Some(FlowCoordinator {
                    agent_id,
                    provider,
                    fencing_token: run.coordinator_fence,
                });
                task.attempt_id = Some(
                    optional_string(&event.data, "attempt_id")
                        .unwrap_or_else(|| format!("attempt:{}", event.event_id)),
                );
            } else if matches!(event.kind, FlowEventKind::TaskStarted) {
                let attempt_id = event
                    .attempt_id
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("task_started requires attempt_id"))?;
                task.attempt_id = Some(attempt_id.to_string());
            } else if matches!(
                event.kind,
                FlowEventKind::Progress
                    | FlowEventKind::Blocked
                    | FlowEventKind::ReadyForReview
                    | FlowEventKind::ReviewApproved
                    | FlowEventKind::ReviewChangesRequested
            ) {
                task.summary =
                    optional_string(&event.data, "summary").or_else(|| task.summary.clone());
                task.blocker = if matches!(event.kind, FlowEventKind::Blocked) {
                    Some(required_string(&event.data, "blocker")?)
                } else if matches!(
                    event.kind,
                    FlowEventKind::ReadyForReview
                        | FlowEventKind::ReviewApproved
                        | FlowEventKind::ReviewChangesRequested
                ) {
                    None
                } else {
                    task.blocker.clone()
                };
                if let Some(evidence) = event.data.get("evidence").and_then(Value::as_array) {
                    task.evidence = evidence
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect();
                }
            }
            task.state = new_state;
            task.revision = event.revision;
        }
        FlowEventKind::FlowCompleted => {
            let transition = template_transition(
                template,
                event,
                Some(&run.stage),
                Some("complete"),
                None,
                None,
            )?;
            if transition.requires_coordinator {
                actor_is_coordinator(
                    run,
                    &event.actor,
                    event.data.get("coordinator_fence").and_then(Value::as_u64),
                )?;
            }
            anyhow::ensure!(
                run.tasks
                    .values()
                    .all(|task| task.state == FlowTaskState::Done),
                "flow cannot complete until every task is review-approved"
            );
            anyhow::ensure!(
                run.stage == "review",
                "flow must be in review stage before completion"
            );
            run.stage = "complete".into();
            run.state = FlowState::Completed;
            cancel_pending_actions(run);
        }
        FlowEventKind::FlowCancelled => {
            actor_is_coordinator(
                run,
                &event.actor,
                event.data.get("coordinator_fence").and_then(Value::as_u64),
            )?;
            run.stage = "cancelled".into();
            run.state = FlowState::Cancelled;
            cancel_pending_actions(run);
        }
        FlowEventKind::ActionRequested => {
            actor_is_coordinator(
                run,
                &event.actor,
                event.data.get("coordinator_fence").and_then(Value::as_u64),
            )?;
            let action_id = required_string(&event.data, "action_id")?;
            anyhow::ensure!(
                !run.pending_actions
                    .iter()
                    .any(|action| action.action_id == action_id),
                "action_id already exists: {action_id}"
            );
            let action = required_string(&event.data, "action")?;
            let payload = event.data.get("payload").cloned().unwrap_or(Value::Null);
            run.pending_actions.push(FlowActionIntent {
                action_id,
                action,
                payload,
                requested_by: event.actor.clone(),
                event_id: event.event_id.clone(),
                coordinator_fence: run.coordinator_fence,
                state: FlowActionState::Pending,
                consumed_by: None,
                consumed_at: None,
            });
        }
        FlowEventKind::ActionConsumed => {
            let action_id = required_string(&event.data, "action_id")?;
            let action = run
                .pending_actions
                .iter_mut()
                .find(|action| action.action_id == action_id)
                .ok_or_else(|| anyhow::anyhow!("unknown action_id: {action_id}"))?;
            anyhow::ensure!(
                action.state == FlowActionState::Pending,
                "action intent was already consumed"
            );
            anyhow::ensure!(
                event.data.get("coordinator_fence").and_then(Value::as_u64)
                    == Some(action.coordinator_fence),
                "action intent coordinator authorization is stale"
            );
            anyhow::ensure!(
                run.coordinator_fence == action.coordinator_fence,
                "action intent was fenced by a coordinator handoff"
            );
            action.state = FlowActionState::Consumed;
            action.consumed_by = Some(event.actor.clone());
            action.consumed_at = Some(event.timestamp.clone());
        }
    }
    Ok(())
}

impl MemoryService {
    pub fn flow_template_list(&self) -> Vec<FlowTemplate> {
        flow_templates()
    }

    pub fn flow_template_get(
        &self,
        template_id: &str,
        version: Option<u32>,
    ) -> anyhow::Result<FlowTemplate> {
        flow_template(template_id, version).ok_or_else(|| {
            anyhow::anyhow!("unknown FlowTemplate {template_id:?} version {version:?}")
        })
    }

    pub fn flow_create(
        &mut self,
        owner: &str,
        workspace: &str,
        board_id: &str,
        template_id: &str,
        template_version: u32,
        goal: &str,
        references: Vec<FlowReference>,
        created_by: &str,
        request_id: &str,
    ) -> anyhow::Result<FlowSnapshot> {
        validate_identifier(owner, "owner", 256)?;
        validate_identifier(workspace, "workspace", 4096)?;
        validate_identifier(board_id, "board_id", 2048)?;
        validate_identifier(goal, "goal", 16 * 1024)?;
        validate_identifier(created_by, "created_by", 256)?;
        validate_identifier(request_id, "request_id", 256)?;
        let template = self.flow_template_get(template_id, Some(template_version))?;
        let board = self
            .board_info(board_id, owner, workspace, None)?
            .ok_or_else(|| anyhow::anyhow!("unknown board_id: {board_id}"))?;
        anyhow::ensure!(
            board.owner == owner && board.workspace == workspace,
            "board is outside requested owner/workspace"
        );
        self.validate_flow_references(owner, workspace, &references)?;
        anyhow::ensure!(
            references.len() <= 100,
            "a FlowRun may have at most 100 references"
        );
        let references_json = serde_json::to_string(&references)?;
        let create_fingerprint = digest(&[
            owner,
            workspace,
            board_id,
            template_id,
            &template_version.to_string(),
            goal,
            created_by,
            &references_json,
        ]);
        for document in self.store.source_documents() {
            if document
                .filters
                .get("doc_kind")
                .is_some_and(|kind| kind == FLOW_RUN_DOC_KIND)
                && document
                    .filters
                    .get(FLOW_OWNER_FILTER)
                    .is_some_and(|value| value == owner)
                && document
                    .filters
                    .get(FLOW_WORKSPACE_FILTER)
                    .is_some_and(|value| value == workspace)
                && document
                    .filters
                    .get(FLOW_CREATE_REQUEST_FILTER)
                    .is_some_and(|value| value == request_id)
            {
                let run = parse_run(document)?;
                let previous_references = serde_json::to_string(&run.references)?;
                let previous_fingerprint = digest(&[
                    &run.owner,
                    &run.workspace,
                    &run.board_id,
                    &run.template_id,
                    &run.template_version.to_string(),
                    &run.goal,
                    &run.created_by,
                    &previous_references,
                ]);
                anyhow::ensure!(
                    previous_fingerprint == create_fingerprint,
                    "create request_id was reused with different FlowRun content"
                );
                return self
                    .flow_get(owner, workspace, &run.flow_id)?
                    .ok_or_else(|| {
                        anyhow::anyhow!("FlowRun disappeared during idempotent lookup")
                    });
            }
        }
        let flow_id = format!("flow_{}", digest(&[owner, workspace, request_id]));
        let timestamp = now();
        let run = FlowRun::new(
            flow_id.clone(),
            owner.into(),
            workspace.into(),
            board_id.into(),
            &template,
            goal.into(),
            references,
            created_by.into(),
            timestamp.clone(),
        );
        let initial = FlowEvent {
            event_id: format!("create:{}", digest(&[request_id])),
            request_id: request_id.into(),
            flow_id: flow_id.clone(),
            revision: 1,
            actor: created_by.into(),
            provider: "agent".into(),
            attempt_id: None,
            kind: FlowEventKind::FlowStarted,
            task_id: None,
            post_id: None,
            data: json!({"template_id": template_id, "template_version": template_version}),
            timestamp: timestamp.clone(),
        };
        let mut run_filters = flow_run_filters(&run);
        run_filters.insert(FLOW_CREATE_REQUEST_FILTER.into(), request_id.into());
        let mut run = run;
        apply_event(&mut run, &template, &initial)?;
        let run_content = serde_json::to_string(&run)?;
        let event_content = serde_json::to_string(&initial)?;
        let run_doc = json_document(
            flow_run_doc_id(&flow_id),
            "flow-run",
            format!("FlowRun {flow_id}"),
            &run_content,
            &timestamp,
            Some(created_by),
            run_filters,
            Some(board_id.into()),
        );
        let event_doc = json_document(
            flow_event_doc_id(&flow_id, &initial.event_id),
            "flow-event",
            format!("Flow event {}", initial.event_id),
            &event_content,
            &timestamp,
            Some(created_by),
            flow_event_filters(&initial, owner, workspace),
            Some(flow_id.clone()),
        );
        self.stage_upsert_batch(vec![run_doc, event_doc])?;
        self.flow_get(owner, workspace, &flow_id)?
            .ok_or_else(|| anyhow::anyhow!("created FlowRun could not be read back"))
    }

    pub fn flow_get(
        &self,
        owner: &str,
        workspace: &str,
        flow_id: &str,
    ) -> anyhow::Result<Option<FlowSnapshot>> {
        validate_identifier(owner, "owner", 256)?;
        validate_identifier(workspace, "workspace", 4096)?;
        validate_identifier(flow_id, "flow_id", 256)?;
        let run_doc_id = flow_run_doc_id(flow_id);
        let Some(document) = self
            .store
            .source_document_by_id(&run_doc_id)
            .filter(|document| {
                document
                    .filters
                    .get("doc_kind")
                    .is_some_and(|kind| kind == FLOW_RUN_DOC_KIND)
                    && document
                        .filters
                        .get(FLOW_OWNER_FILTER)
                        .is_some_and(|value| value == owner)
                    && document
                        .filters
                        .get(FLOW_WORKSPACE_FILTER)
                        .is_some_and(|value| value == workspace)
            })
        else {
            return Ok(None);
        };
        let run = parse_run(document)?;
        let template = self.flow_template_get(&run.template_id, Some(run.template_version))?;
        let mut events = self.flow_events_for(&run, owner, workspace)?;
        events.sort_by(|left, right| {
            left.revision
                .cmp(&right.revision)
                .then_with(|| left.event_id.cmp(&right.event_id))
        });
        anyhow::ensure!(
            events
                .first()
                .is_some_and(|event| event.kind == FlowEventKind::FlowStarted),
            "FlowRun has no creation event"
        );
        for (index, event) in events.iter().enumerate() {
            anyhow::ensure!(
                event.revision == (index as u64) + 1,
                "FlowRun event timeline has a revision gap"
            );
            anyhow::ensure!(
                event.flow_id == run.flow_id,
                "FlowEvent belongs to a different FlowRun"
            );
        }
        anyhow::ensure!(
            run.revision == events.len() as u64,
            "FlowRun projection revision does not match event timeline"
        );
        // Resolve the pinned template so a corrupt projection cannot hide a removed version.
        let _ = template;
        Ok(Some(FlowSnapshot { run, events }))
    }

    pub fn flow_list(&self, owner: &str, workspace: &str) -> anyhow::Result<Vec<FlowSnapshot>> {
        validate_identifier(owner, "owner", 256)?;
        validate_identifier(workspace, "workspace", 4096)?;
        let mut flow_ids = Vec::new();
        for document in self.store.source_documents() {
            if document
                .filters
                .get("doc_kind")
                .is_some_and(|kind| kind == FLOW_RUN_DOC_KIND)
                && document
                    .filters
                    .get(FLOW_OWNER_FILTER)
                    .is_some_and(|value| value == owner)
                && document
                    .filters
                    .get(FLOW_WORKSPACE_FILTER)
                    .is_some_and(|value| value == workspace)
            {
                flow_ids.push(parse_run(&document)?.flow_id);
            }
        }
        flow_ids.sort();
        flow_ids.dedup();
        flow_ids
            .into_iter()
            .filter_map(|flow_id| match self.flow_get(owner, workspace, &flow_id) {
                Ok(Some(snapshot)) => Some(Ok(snapshot)),
                Ok(None) => None,
                Err(error) => Some(Err(error)),
            })
            .collect()
    }

    pub fn flow_event_append(&mut self, input: FlowEventAppend) -> anyhow::Result<FlowSnapshot> {
        validate_identifier(&input.event_id, "event_id", 256)?;
        validate_identifier(&input.request_id, "request_id", 256)?;
        validate_identifier(&input.actor, "actor", 256)?;
        validate_identifier(&input.provider, "provider", 64)?;
        anyhow::ensure!(
            serde_json::to_vec(&input.data)?.len() <= 32 * 1024,
            "event data exceeds 32768 bytes"
        );
        let mut data = input.data.clone();
        if let Some(fence) = input.coordinator_fence {
            let object = data.as_object_mut().ok_or_else(|| {
                anyhow::anyhow!("coordinator events require event data to be a JSON object")
            })?;
            object.insert("coordinator_fence".into(), json!(fence));
        }
        let mut snapshot = self
            .flow_get(&input.owner, &input.workspace, &input.flow_id)?
            .ok_or_else(|| anyhow::anyhow!("unknown flow_id: {}", input.flow_id))?;
        if let Some(existing) = snapshot
            .events
            .iter()
            .find(|event| event.event_id == input.event_id || event.request_id == input.request_id)
        {
            let same = existing.event_id == input.event_id
                && existing.request_id == input.request_id
                && existing.actor == input.actor
                && existing.provider == input.provider
                && existing.attempt_id == input.attempt_id
                && existing.kind == input.kind
                && existing.task_id == input.task_id
                && existing.post_id == input.post_id
                && existing.data == data;
            anyhow::ensure!(
                same,
                "event_id or request_id was reused with different FlowEvent content"
            );
            return Ok(snapshot);
        }
        anyhow::ensure!(
            snapshot.run.revision == input.expected_revision,
            "FlowRun revision conflict: expected {}, current {}",
            input.expected_revision,
            snapshot.run.revision
        );
        let event = FlowEvent {
            event_id: input.event_id,
            request_id: input.request_id,
            flow_id: input.flow_id,
            revision: snapshot.run.revision + 1,
            actor: input.actor,
            provider: input.provider,
            attempt_id: input.attempt_id,
            kind: input.kind,
            task_id: input.task_id,
            post_id: input.post_id,
            data,
            timestamp: now(),
        };
        let template = self.flow_template_get(
            &snapshot.run.template_id,
            Some(snapshot.run.template_version),
        )?;
        self.validate_event_references(&input.owner, &input.workspace, &event)?;
        apply_event(&mut snapshot.run, &template, &event)?;
        let event_content = serde_json::to_string(&event)?;
        let doc = json_document(
            flow_event_doc_id(&event.flow_id, &event.event_id),
            "flow-event",
            format!("Flow event {}", event.event_id),
            &event_content,
            &event.timestamp,
            Some(&event.actor),
            flow_event_filters(&event, &input.owner, &input.workspace),
            Some(event.flow_id.clone()),
        );
        let run_content = serde_json::to_string(&snapshot.run)?;
        let mut run_filters = flow_run_filters(&snapshot.run);
        run_filters.insert(
            FLOW_CREATE_REQUEST_FILTER.into(),
            snapshot
                .events
                .first()
                .map(|initial| initial.request_id.clone())
                .ok_or_else(|| anyhow::anyhow!("FlowRun creation event is missing"))?,
        );
        let run_doc = json_document(
            flow_run_doc_id(&snapshot.run.flow_id),
            "flow-run",
            format!("FlowRun {}", snapshot.run.flow_id),
            &run_content,
            &event.timestamp,
            Some(&snapshot.run.created_by),
            run_filters,
            Some(snapshot.run.board_id.clone()),
        );
        self.stage_upsert_batch(vec![run_doc, doc])?;
        self.flow_get(&input.owner, &input.workspace, &event.flow_id)?
            .ok_or_else(|| anyhow::anyhow!("FlowRun disappeared after event append"))
    }

    pub fn flow_pending_actions(
        &self,
        owner: &str,
        workspace: &str,
        flow_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<FlowActionIntent>> {
        let snapshot = self
            .flow_get(owner, workspace, flow_id)?
            .ok_or_else(|| anyhow::anyhow!("unknown flow_id: {flow_id}"))?;
        anyhow::ensure!(
            snapshot.run.state == FlowState::Active,
            "Flow is {:?}; pending actions are unavailable",
            snapshot.run.state
        );
        Ok(snapshot
            .run
            .pending_actions
            .into_iter()
            .filter(|action| action.state == FlowActionState::Pending)
            .take(limit.clamp(1, 1000))
            .collect())
    }

    pub fn flow_action_consume(
        &mut self,
        owner: &str,
        workspace: &str,
        flow_id: &str,
        action_id: &str,
        expected_revision: u64,
        coordinator_fence: u64,
        consumer: &str,
        provider: &str,
        request_id: &str,
    ) -> anyhow::Result<FlowSnapshot> {
        let snapshot = self
            .flow_get(owner, workspace, flow_id)?
            .ok_or_else(|| anyhow::anyhow!("unknown flow_id: {flow_id}"))?;
        anyhow::ensure!(
            snapshot.run.state == FlowState::Active,
            "Flow is {:?}; pending actions cannot be consumed",
            snapshot.run.state
        );
        snapshot.run.coordinator.as_ref().ok_or_else(|| {
            anyhow::anyhow!("this action requires an active coordinator authorization")
        })?;
        anyhow::ensure!(
            coordinator_fence == snapshot.run.coordinator_fence,
            "stale coordinator fencing token; current token is {}",
            snapshot.run.coordinator_fence
        );
        validate_identifier(action_id, "action_id", 256)?;
        let event_id = format!("consume:{}", digest(&[flow_id, action_id, request_id]));
        self.flow_event_append(FlowEventAppend {
            owner: owner.into(),
            workspace: workspace.into(),
            flow_id: flow_id.into(),
            expected_revision,
            coordinator_fence: Some(coordinator_fence),
            event_id,
            request_id: request_id.into(),
            actor: consumer.into(),
            provider: provider.into(),
            attempt_id: None,
            kind: FlowEventKind::ActionConsumed,
            task_id: None,
            post_id: None,
            data: json!({"action_id": action_id, "coordinator_fence": coordinator_fence}),
        })
    }

    fn flow_events_for(
        &self,
        run: &FlowRun,
        owner: &str,
        workspace: &str,
    ) -> anyhow::Result<Vec<FlowEvent>> {
        self.store
            .source_documents()
            .into_iter()
            .filter(|document| {
                let matches = document
                    .filters
                    .get("doc_kind")
                    .is_some_and(|kind| kind == FLOW_EVENT_DOC_KIND)
                    && document
                        .filters
                        .get(FLOW_ID_FILTER)
                        .is_some_and(|value| value == &run.flow_id)
                    && document
                        .filters
                        .get(FLOW_OWNER_FILTER)
                        .is_some_and(|value| value == owner)
                    && document
                        .filters
                        .get(FLOW_WORKSPACE_FILTER)
                        .is_some_and(|value| value == workspace);
                matches
            })
            .map(|document| parse_event(document))
            .collect()
    }

    fn validate_flow_references(
        &mut self,
        owner: &str,
        workspace: &str,
        references: &[FlowReference],
    ) -> anyhow::Result<()> {
        for reference in references {
            validate_identifier(&reference.id, "reference id", 2048)?;
            validate_identifier(&reference.relation, "reference relation", 64)?;
            match reference.kind {
                FlowReferenceKind::Board => {
                    let board = self
                        .board_info(&reference.id, owner, workspace, None)?
                        .ok_or_else(|| anyhow::anyhow!("unknown linked board: {}", reference.id))?;
                    anyhow::ensure!(
                        board.owner == owner && board.workspace == workspace,
                        "linked board is outside requested owner/workspace"
                    );
                }
                FlowReferenceKind::Post => {
                    let (board_id, _) = reference.id.split_once(":post:").ok_or_else(|| {
                        anyhow::anyhow!("post reference must include its board id")
                    })?;
                    let board = self
                        .board_info(board_id, owner, workspace, None)?
                        .ok_or_else(|| {
                            anyhow::anyhow!("linked post belongs to unknown board: {board_id}")
                        })?;
                    anyhow::ensure!(
                        self.board_get(Some(board_id), owner, workspace, None, &reference.id)?
                            .is_some(),
                        "unknown linked post: {}",
                        reference.id
                    );
                    anyhow::ensure!(
                        board.owner == owner && board.workspace == workspace,
                        "linked post is outside requested owner/workspace"
                    );
                }
            }
        }
        Ok(())
    }

    fn validate_event_references(
        &mut self,
        owner: &str,
        workspace: &str,
        event: &FlowEvent,
    ) -> anyhow::Result<()> {
        let task_lifecycle_event = matches!(
            event.kind,
            FlowEventKind::TaskCreated
                | FlowEventKind::TaskAssigned
                | FlowEventKind::TaskStarted
                | FlowEventKind::Progress
                | FlowEventKind::Blocked
                | FlowEventKind::ReadyForReview
                | FlowEventKind::ReviewApproved
                | FlowEventKind::ReviewChangesRequested
                | FlowEventKind::TaskCancelled
        );
        if task_lifecycle_event {
            anyhow::ensure!(
                event.post_id.is_some(),
                "task lifecycle events require post_id"
            );
        }
        if let Some(post_id) = event.post_id.as_deref() {
            self.validate_flow_references(
                owner,
                workspace,
                &[FlowReference {
                    kind: FlowReferenceKind::Post,
                    id: post_id.to_string(),
                    relation: format!("task_report:{}", event.task_id.as_deref().unwrap_or("flow")),
                }],
            )?;
        }
        if matches!(event.kind, FlowEventKind::ReferenceAdded) {
            let reference: FlowReference =
                serde_json::from_value(
                    event.data.get("reference").cloned().ok_or_else(|| {
                        anyhow::anyhow!("reference_added requires data.reference")
                    })?,
                )?;
            self.validate_flow_references(owner, workspace, &[reference])?;
        }
        Ok(())
    }
}
