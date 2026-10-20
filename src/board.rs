//! Agent bulletin board: short status updates agents post and read via MCP.
//!
//! A board is scoped to one owner + workspace + caller-supplied key (e.g.
//! `pr-81-review`). Agents discover boards with [`MemoryService::board_list`]
//! or idempotently create-or-get with [`MemoryService::board_open`]; content
//! operations take an optional `board_id`, defaulting to the current
//! session's board.
//!
//! The default board is a session-scoped alias, not a fixed board: the
//! default board *points to* the board whose ID is derived from the
//! conversation session ID (`session_board_id`). Each session therefore
//! gets its own default board automatically — posts from different
//! sessions never intermingle. The alias name is `"default"`; session
//! boards live under the reserved key namespace `"session:"`, which
//! `board_open` rejects for caller-supplied keys.
//!
//! Storage: boards and posts are [`SourceDocument`]s in the shared
//! [`IndexStore`], distinguished by a `doc_kind` filter (`"board"` vs
//! `"board_post"`). No schema change, no separate database. The single
//! writer path (PR #81) orders mutations; `board_post` assigns the next
//! per-board sequence while holding the board lock.
//!
//! Storage: boards and posts are [`SourceDocument`]s in the shared
//! [`IndexStore`], distinguished by a `doc_kind` filter (`"board"` vs
//! `"board_post"`). No schema change, no separate database. The single
//! writer path (PR #81) orders mutations; `board_post` assigns the next
//! per-board sequence while holding the board lock.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Filter key marking a document's kind in the board subsystem.
pub const BOARD_DOC_KIND_FILTER: &str = "doc_kind";
/// `doc_kind` value for board definition documents.
pub const BOARD_DOC_KIND: &str = "board";
/// `doc_kind` value for board post documents.
pub const BOARD_POST_KIND: &str = "board_post";

/// Reserved board key for the per-(owner, workspace) default board.
/// `board_open` accepts this key like any other; it resolves to the same
/// board the content tools use when `board_id` is omitted.
pub const DEFAULT_BOARD_KEY: &str = "default";

/// Reserved key namespace for session boards. `board_open` rejects
/// caller-supplied keys starting with this prefix so user keys can never
/// collide with a session board's key.
pub const SESSION_BOARD_KEY_PREFIX: &str = "session:";

/// Title used when a session board's persistent record is first created.
pub const DEFAULT_BOARD_TITLE: &str = "Default board";

/// Whether a `board_id` argument is the default-board alias (`"default"`).
pub fn is_default_alias(board_id: &str) -> bool {
    board_id.trim() == DEFAULT_BOARD_KEY
}

/// The board key for a session's board. Reserved: `board_open` refuses
/// caller keys with this prefix.
pub fn session_board_key(session_id: &str) -> String {
    format!("{SESSION_BOARD_KEY_PREFIX}{session_id}")
}

/// Deterministic board ID for a session's board.
///
/// Built on [`board_id_for`] with the reserved `session:` key namespace,
/// so it can never equal a user-keyed board ID (`board_open` rejects
/// caller keys starting with `"session:"`, and `:` is escaped in keys).
pub fn session_board_id(owner: &str, workspace: &str, session_id: &str) -> String {
    board_id_for(owner, workspace, &session_board_key(session_id))
}

/// The default board *points to* the current session's board: omitting
/// `board_id` (or passing the `"default"` alias) resolves to this ID.
pub fn default_board_id(owner: &str, workspace: &str, session_id: &str) -> String {
    session_board_id(owner, workspace, session_id)
}

/// Construct the default board value without persisting it. The persistent
/// record is created on first post; until then the board exists
/// conceptually (empty) so reads and `board_list` behave sensibly.
pub fn default_board(owner: &str, workspace: &str, session_id: &str) -> Board {
    Board {
        board_id: default_board_id(owner, workspace, session_id),
        owner: owner.to_string(),
        workspace: workspace.to_string(),
        key: DEFAULT_BOARD_KEY.to_string(),
        title: format!("{DEFAULT_BOARD_TITLE} (session {session_id})"),
        board_type: DEFAULT_BOARD_TYPE.to_string(),
        links: Vec::new(),
        template_id: None,
        template_version: None,
        // Synthetic boards predate any persistent record; the real
        // timestamp is set when the record is created.
        created_at: String::new(),
    }
}

/// Whether a board ID is the default board for (owner, workspace,
/// session_id) — i.e. that session's board.
pub fn is_default_board(board_id: &str, owner: &str, workspace: &str, session_id: &str) -> bool {
    board_id == default_board_id(owner, workspace, session_id)
}

/// Filter keys used on board and post documents.
pub const BOARD_ID_FILTER: &str = "board_id";
pub const BOARD_OWNER_FILTER: &str = "board_owner";
pub const BOARD_WORKSPACE_FILTER: &str = "board_workspace";
pub const BOARD_KEY_FILTER: &str = "board_key";
pub const BOARD_AUTHOR_FILTER: &str = "author_agent_id";
pub const BOARD_PROVIDER_FILTER: &str = "provider";
pub const BOARD_SEQUENCE_FILTER: &str = "sequence";
pub const BOARD_REQUEST_ID_FILTER: &str = "request_id";
pub const BOARD_TYPE_FILTER: &str = "board_type";
pub const BOARD_POST_TITLE_FILTER: &str = "board_post_title";
pub const BOARD_POST_LINKS_FILTER: &str = "board_post_links";
pub const BOARD_POST_SESSION_KEY_FILTER: &str = "board_post_session_key";
pub const BOARD_TEMPLATE_ID_FILTER: &str = "board_template_id";
pub const BOARD_POST_TEMPLATE_ID_FILTER: &str = "board_post_template_id";
pub const BOARD_POST_TEMPLATE_VERSION_FILTER: &str = "board_post_template_version";
pub const BOARD_POST_FIELDS_FILTER: &str = "board_post_fields";

pub const DEFAULT_BOARD_TYPE: &str = "general";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BoardTemplateFieldType {
    String,
    StringList,
    Boolean,
    Integer,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardTemplateField {
    pub name: String,
    pub description: String,
    #[serde(rename = "type")]
    pub field_type: BoardTemplateFieldType,
    pub required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardPostTemplate {
    pub id: String,
    pub version: u32,
    pub name: String,
    pub description: String,
    pub fields: Vec<BoardTemplateField>,
}

/// A versioned, declarative contract for structured board posts.
/// Templates describe shape and agent guidance, not workflow transitions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardTemplate {
    pub id: String,
    pub version: u32,
    pub name: String,
    pub description: String,
    #[serde(rename = "type")]
    pub board_type: String,
    pub instructions: String,
    pub post_templates: Vec<BoardPostTemplate>,
}

fn field(
    name: &str,
    description: &str,
    field_type: BoardTemplateFieldType,
    required: bool,
) -> BoardTemplateField {
    BoardTemplateField {
        name: name.into(),
        description: description.into(),
        field_type,
        required,
    }
}

fn post_template(
    id: &str,
    name: &str,
    description: &str,
    fields: Vec<BoardTemplateField>,
) -> BoardPostTemplate {
    BoardPostTemplate {
        id: id.into(),
        version: 1,
        name: name.into(),
        description: description.into(),
        fields,
    }
}

/// Built-in templates are versioned with the server and available to every
/// connected MCP agent.
fn board_template_versions() -> Vec<BoardTemplate> {
    use BoardTemplateFieldType::{String as Text, StringList};

    vec![
        BoardTemplate {
            id: "project".into(),
            version: 1,
            name: "Project plan".into(),
            description: "Capture a project goal, boundaries, success criteria, and staged outcomes.".into(),
            board_type: "project".into(),
            instructions: "Start with a project_brief. Add one stage post per meaningful outcome and link planning, task, and milestone boards. Ask the user about material missing constraints before treating the plan as ready.".into(),
            post_templates: vec![
                post_template("project_brief", "Project brief", "The agreed project goal and boundaries.", vec![
                    field("goal", "What outcome should the project achieve?", Text, true),
                    field("success_criteria", "Observable conditions that show the goal was met.", StringList, true),
                    field("scope", "Work included in this project.", Text, false),
                    field("constraints", "Known limits, dependencies, or requirements.", StringList, false),
                    field("non_goals", "Related work explicitly outside scope.", StringList, false),
                ]),
                post_template("stage", "Stage", "A project stage and its intended outcome.", vec![
                    field("name", "Short stage name.", Text, true),
                    field("outcome", "What this stage will deliver.", Text, true),
                    field("exit_criteria", "Checks that establish the stage outcome.", StringList, true),
                    field("dependencies", "Earlier stages or external prerequisites.", StringList, false),
                ]),
            ],
        },
        BoardTemplate {
            id: "planning".into(),
            version: 1,
            name: "Plan and decisions".into(),
            description: "Record a reviewable plan, assumptions, risks, and decisions.".into(),
            board_type: "planning".into(),
            instructions: "Post the plan before decomposing it into delegated work. Separate confirmed facts from assumptions, and link evidence or decisions that affect the plan.".into(),
            post_templates: vec![
                post_template("plan", "Plan", "A proposed approach that can be reviewed before execution.", vec![
                    field("summary", "The proposed approach and why it meets the goal.", Text, true),
                    field("assumptions", "Unverified premises that could change the plan.", StringList, false),
                    field("risks", "Likely failure modes and their impact.", StringList, false),
                    field("dependencies", "Required decisions, systems, or earlier work.", StringList, false),
                ]),
                post_template("decision", "Decision", "A decision that changes or constrains the plan.", vec![
                    field("decision", "What was decided.", Text, true),
                    field("reason", "Why this option was selected.", Text, true),
                    field("alternatives", "Other options considered.", StringList, false),
                ]),
            ],
        },
        BoardTemplate {
            id: "task".into(),
            version: 1,
            name: "Task handoff".into(),
            description: "Create executable task handoffs and record progress or results.".into(),
            board_type: "tasks".into(),
            instructions: "Before handing work to another agent, include bounded scope, testable acceptance criteria, dependencies, and an intended owner. Link the parent plan and milestone. Workers should post progress, blockers, and evidence here.".into(),
            post_templates: vec![
                post_template("task", "Task", "A bounded piece of work ready for an agent handoff.", vec![
                    field("scope", "What the assignee should change or investigate.", Text, true),
                    field("acceptance_criteria", "Checks that determine whether the task is complete.", StringList, true),
                    field("owner", "Intended agent or person responsible for the work.", Text, true),
                    field("dependencies", "Work or decisions required before starting.", StringList, false),
                ]),
                post_template("progress", "Progress update", "A concise update from the assigned worker.", vec![
                    field("summary", "What changed or was learned.", Text, true),
                    field("next_step", "The next concrete action, if any.", Text, false),
                ]),
                post_template("blocker", "Blocker", "A blocker requiring attention.", vec![
                    field("description", "What is blocking progress.", Text, true),
                    field("needed", "Decision or action needed to continue.", Text, true),
                ]),
                post_template("result", "Result", "A result with verification evidence.", vec![
                    field("summary", "What was completed and the outcome.", Text, true),
                    field("evidence", "Commands, reports, commits, or other verification references.", StringList, true),
                ]),
            ],
        },
        BoardTemplate {
            id: "milestone".into(),
            version: 1,
            name: "Milestone and exit review".into(),
            description: "Define milestone outcomes and record evidence against exit criteria.".into(),
            board_type: "milestone".into(),
            instructions: "State measurable exit criteria when creating the milestone. Record the observed result and link evidence when reviewing it. The board validates post shape but does not decide whether the milestone passed.".into(),
            post_templates: vec![
                post_template("milestone", "Milestone", "The planned milestone outcome and its exit criteria.", vec![
                    field("outcome", "The result this milestone represents.", Text, true),
                    field("exit_criteria", "Observable checks required for the outcome.", StringList, true),
                    field("dependencies", "Required tasks, decisions, or earlier milestones.", StringList, false),
                ]),
                post_template("review", "Review", "An evidence-backed review of milestone criteria.", vec![
                    field("summary", "What the evidence shows.", Text, true),
                    field("evidence", "Links or references supporting the review.", StringList, true),
                    field("open_items", "Remaining gaps or follow-up work.", StringList, false),
                ]),
            ],
        },
    ]
}

/// Return the latest version of each built-in template for discovery.
pub fn board_templates() -> Vec<BoardTemplate> {
    let mut latest = BTreeMap::<String, BoardTemplate>::new();
    for template in board_template_versions() {
        let replace = latest
            .get(&template.id)
            .map_or(true, |existing| template.version > existing.version);
        if replace {
            latest.insert(template.id.clone(), template);
        }
    }
    latest.into_values().collect()
}

pub fn board_template(template_id: &str) -> Option<BoardTemplate> {
    board_templates()
        .into_iter()
        .find(|template| template.id == template_id)
}

/// Look up an exact template version so existing templated boards retain
/// their original contract after a newer version is introduced.
pub fn board_template_version(template_id: &str, version: u32) -> Option<BoardTemplate> {
    board_template_versions()
        .into_iter()
        .find(|template| template.id == template_id && template.version == version)
}

/// Validate a structured post against a board's versioned built-in template.
/// Unknown fields are rejected to catch misspellings and stale agents.
pub fn validate_template_post(
    template: &BoardTemplate,
    post_template_id: &str,
    post_template_version: u32,
    fields: &Value,
) -> Result<(), Vec<String>> {
    let Some(post_template) = template
        .post_templates
        .iter()
        .find(|item| item.id == post_template_id && item.version == post_template_version)
    else {
        return Err(vec![format!(
            "unknown post template {post_template_id:?} version {post_template_version} for board template {:?}",
            template.id,
        )]);
    };
    let Some(values) = fields.as_object() else {
        return Err(vec!["fields must be a JSON object".into()]);
    };
    let mut errors = Vec::new();
    for field in &post_template.fields {
        let Some(value) = values.get(&field.name) else {
            if field.required {
                errors.push(format!("missing required field {:?}", field.name));
            }
            continue;
        };
        let valid = match field.field_type {
            BoardTemplateFieldType::String => {
                value.as_str().is_some_and(|text| !text.trim().is_empty())
            }
            BoardTemplateFieldType::StringList => value.as_array().is_some_and(|items| {
                !items.is_empty()
                    && items
                        .iter()
                        .all(|item| item.as_str().is_some_and(|text| !text.trim().is_empty()))
            }),
            BoardTemplateFieldType::Boolean => value.is_boolean(),
            BoardTemplateFieldType::Integer => value.as_i64().is_some(),
        };
        if !valid {
            let expected = match field.field_type {
                BoardTemplateFieldType::String => "non-empty string",
                BoardTemplateFieldType::StringList => "non-empty list of non-empty strings",
                BoardTemplateFieldType::Boolean => "boolean",
                BoardTemplateFieldType::Integer => "integer",
            };
            errors.push(format!("field {:?} must be a {expected}", field.name));
        }
    }
    for key in values.keys() {
        if !post_template.fields.iter().any(|field| field.name == *key) {
            errors.push(format!(
                "unknown field {key:?} for post template {post_template_id:?}"
            ));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// A generic relation from a board or post to another board, post, or
/// reference. The relation label is supplied by the agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BoardLink {
    pub target: String,
    pub relation: String,
}

/// One board and its ordered posts for read-only dashboard rendering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardSnapshot {
    pub board: Board,
    pub posts: Vec<BoardPost>,
    pub has_more: bool,
}

/// A bulletin board: shared by a parent agent and its subagents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Board {
    /// Server-assigned, stable for (owner, workspace, key).
    pub board_id: String,
    /// Stable owner of the memory (user_id).
    pub owner: String,
    /// Workspace scope (canonical project root).
    pub workspace: String,
    /// Caller-supplied key, unique within (owner, workspace).
    pub key: String,
    pub title: String,
    /// Descriptive metadata only; it does not change board behavior.
    #[serde(default = "default_board_type", rename = "type")]
    pub board_type: String,
    #[serde(default)]
    pub links: Vec<BoardLink>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_version: Option<u32>,
    pub created_at: String,
}

fn default_board_type() -> String {
    DEFAULT_BOARD_TYPE.to_string()
}

/// One post on a board.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardPost {
    /// Document ID of the stored post.
    pub post_id: String,
    pub board_id: String,
    pub author_agent_id: String,
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Opaque telemetry session key, when the writer session is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_key: Option<String>,
    pub content: String,
    #[serde(default)]
    pub links: Vec<BoardLink>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_template_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_template_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fields: Option<Value>,
    /// Increasing within the board, assigned by the service.
    pub sequence: u64,
    pub created_at: String,
}

/// Deterministic board ID for (owner, workspace, key).
///
/// Stable across restarts and processes, so concurrent `board_open` calls
/// with the same key converge on one board without coordination.
pub fn board_id_for(owner: &str, workspace: &str, key: &str) -> String {
    // Sanitize each component so the composite ID is unambiguous.
    fn esc(s: &str) -> String {
        s.replace('%', "%25").replace(':', "%3A")
    }
    format!("board:{}:{}:{}", esc(owner), esc(workspace), esc(key))
}

/// Document ID for a board definition document.
pub fn board_doc_id(board_id: &str) -> String {
    format!("{board_id}:definition")
}

/// Document ID for a post: includes the sequence so IDs are naturally
/// ordered and unique within the board.
pub fn board_post_doc_id(board_id: &str, sequence: u64) -> String {
    format!("{board_id}:post:{sequence:020}")
}

/// Build the [`SourceDocument`] filters for a board definition.
pub fn board_doc_filters(board: &Board) -> BTreeMap<String, String> {
    let mut f = BTreeMap::new();
    f.insert(
        BOARD_DOC_KIND_FILTER.to_string(),
        BOARD_DOC_KIND.to_string(),
    );
    f.insert(BOARD_ID_FILTER.to_string(), board.board_id.clone());
    f.insert(BOARD_OWNER_FILTER.to_string(), board.owner.clone());
    f.insert(BOARD_WORKSPACE_FILTER.to_string(), board.workspace.clone());
    f.insert(BOARD_KEY_FILTER.to_string(), board.key.clone());
    f.insert(BOARD_TYPE_FILTER.to_string(), board.board_type.clone());
    if let Some(template_id) = &board.template_id {
        f.insert(BOARD_TEMPLATE_ID_FILTER.to_string(), template_id.clone());
    }
    f
}

/// Build the [`SourceDocument`] filters for a post.
pub fn board_post_doc_filters(post: &BoardPost, request_id: &str) -> BTreeMap<String, String> {
    let mut f = BTreeMap::new();
    f.insert(
        BOARD_DOC_KIND_FILTER.to_string(),
        BOARD_POST_KIND.to_string(),
    );
    f.insert(BOARD_ID_FILTER.to_string(), post.board_id.clone());
    f.insert(
        BOARD_AUTHOR_FILTER.to_string(),
        post.author_agent_id.clone(),
    );
    f.insert(BOARD_PROVIDER_FILTER.to_string(), post.provider.clone());
    f.insert(BOARD_SEQUENCE_FILTER.to_string(), post.sequence.to_string());
    if let Some(title) = &post.title {
        f.insert(BOARD_POST_TITLE_FILTER.to_string(), title.clone());
    }
    if let Some(session_key) = &post.session_key {
        f.insert(
            BOARD_POST_SESSION_KEY_FILTER.to_string(),
            session_key.clone(),
        );
    }
    if !post.links.is_empty() {
        if let Ok(links) = serde_json::to_string(&post.links) {
            f.insert(BOARD_POST_LINKS_FILTER.to_string(), links);
        }
    }
    if let Some(template_id) = &post.post_template_id {
        f.insert(
            BOARD_POST_TEMPLATE_ID_FILTER.to_string(),
            template_id.clone(),
        );
    }
    if let Some(version) = post.post_template_version {
        f.insert(
            BOARD_POST_TEMPLATE_VERSION_FILTER.to_string(),
            version.to_string(),
        );
    }
    if let Some(fields) = &post.fields {
        if let Ok(value) = serde_json::to_string(fields) {
            f.insert(BOARD_POST_FIELDS_FILTER.to_string(), value);
        }
    }
    if !request_id.is_empty() {
        f.insert(BOARD_REQUEST_ID_FILTER.to_string(), request_id.to_string());
    }
    f
}

/// Serialize a board definition into document content (JSON).
pub fn board_doc_content(board: &Board) -> String {
    serde_json::to_string(board).unwrap_or_else(|_| "{}".to_string())
}

/// Parse a board definition back from document content.
pub fn board_from_doc_content(content: &str) -> Option<Board> {
    serde_json::from_str(content).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn board_id_is_stable_and_unambiguous() {
        let a = board_id_for("u", "ws", "k");
        let b = board_id_for("u", "ws", "k");
        assert_eq!(a, b);
        // ':' in components must not collide with separators.
        let tricky = board_id_for("u", "w:s", "k");
        let plain = board_id_for("u", "w", "s:k");
        assert_ne!(tricky, plain);
    }

    #[test]
    fn post_doc_ids_order_lexicographically_by_sequence() {
        let b = "board:u:ws:k";
        assert!(board_post_doc_id(b, 2) < board_post_doc_id(b, 10));
    }

    #[test]
    fn session_board_id_is_stable_and_session_scoped() {
        let a = session_board_id("u", "ws", "s1");
        let b = session_board_id("u", "ws", "s1");
        assert_eq!(a, b);
        // Different sessions get different boards.
        assert_ne!(a, session_board_id("u", "ws", "s2"));
        // The session board ID is exactly the board ID for the reserved
        // "session:{sid}" key — which is why board_open rejects
        // caller-supplied keys with the "session:" prefix.
        assert_eq!(a, board_id_for("u", "ws", "session:s1"));
        // The default alias points at the session board.
        assert_eq!(default_board_id("u", "ws", "s1"), a);
        assert!(is_default_board(&a, "u", "ws", "s1"));
        assert!(!is_default_board(&a, "u", "ws", "s2"));
        assert!(is_default_alias("default"));
        assert!(is_default_alias("  default  "));
        assert!(!is_default_alias(&a));
    }
}
