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
use serde_json::{json, Value};

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
        template_instance: None,
        fields: None,
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
pub enum TemplateFieldType {
    String,
    StringList,
    Boolean,
    Integer,
    Number,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateField {
    pub id: String,
    #[serde(rename = "type")]
    pub field_type: TemplateFieldType,
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, rename = "enum", skip_serializing_if = "Option::is_none")]
    pub enum_values: Option<Vec<Value>>,
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateSchema {
    pub version: u32,
    pub fields: Vec<TemplateField>,
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplatePresentation {
    pub version: u32,
    #[serde(default)]
    pub components: Vec<Value>,
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostTemplateSnapshot {
    pub template_id: String,
    pub version: u32,
    pub content_hash: String,
    pub schema: TemplateSchema,
    pub presentation: TemplatePresentation,
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateSnapshot {
    pub schema: TemplateSchema,
    pub presentation: TemplatePresentation,
    #[serde(default)]
    pub post_templates: Vec<PostTemplateSnapshot>,
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, Value>,
}

/// Immutable template identity and snapshot supplied by the template owner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateInstance {
    pub contract_version: u32,
    pub issuer: String,
    pub template_id: String,
    pub version: u32,
    pub content_hash: String,
    pub snapshot: TemplateSnapshot,
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, Value>,
}

pub const TEMPLATE_SNAPSHOT_MAX_BYTES: usize = 64 * 1024;
pub const TEMPLATE_FIELDS_MAX_BYTES: usize = 32 * 1024;

fn content_hash(value: &Value) -> anyhow::Result<String> {
    use sha2::Digest;
    let bytes = serde_jcs::to_vec(value)?;
    Ok(format!("sha256:{:x}", sha2::Sha256::digest(bytes)))
}

fn validate_schema(schema: &TemplateSchema) -> anyhow::Result<()> {
    if schema.version != 1 {
        anyhow::bail!("unsupported template schema version: {}", schema.version);
    }
    if schema.fields.len() > 256 {
        anyhow::bail!("template schema may define at most 256 fields");
    }
    let mut ids = std::collections::HashSet::new();
    for field in &schema.fields {
        if field.id.trim().is_empty() || field.id.len() > 128 || !ids.insert(&field.id) {
            anyhow::bail!("template field IDs must be unique and 1-128 bytes");
        }
        if field.label.as_ref().is_some_and(|s| s.len() > 256)
            || field.description.as_ref().is_some_and(|s| s.len() > 2048)
        {
            anyhow::bail!("template field label/description exceeds its size limit");
        }
        if let Some(values) = &field.enum_values {
            if values.len() > 256 {
                anyhow::bail!("template field enum may contain at most 256 values");
            }
        }
    }
    Ok(())
}

pub fn validate_template_instance(instance: &TemplateInstance) -> anyhow::Result<()> {
    if instance.contract_version != 1 || instance.version == 0 {
        anyhow::bail!("unsupported template instance contract or version");
    }
    if instance.issuer.trim().is_empty()
        || instance.issuer.len() > 128
        || instance.template_id.trim().is_empty()
        || instance.template_id.len() > 128
    {
        anyhow::bail!("template issuer and ID must be 1-128 bytes");
    }
    let snapshot_json = serde_json::to_value(&instance.snapshot)?;
    if serde_json::to_vec(&snapshot_json)?.len() > TEMPLATE_SNAPSHOT_MAX_BYTES {
        anyhow::bail!("template snapshot exceeds 65536 bytes");
    }
    validate_snapshot_depth(&snapshot_json, 0)?;
    validate_schema(&instance.snapshot.schema)?;
    if instance.snapshot.post_templates.len() > 100 {
        anyhow::bail!("template snapshot may contain at most 100 post templates");
    }
    validate_presentation(
        &instance.snapshot.presentation,
        &instance.snapshot.schema,
        Some(&instance.snapshot.post_templates),
    )?;
    let mut refs = std::collections::HashSet::new();
    for post in &instance.snapshot.post_templates {
        if post.template_id.trim().is_empty()
            || post.template_id.len() > 128
            || post.version == 0
            || !refs.insert((post.template_id.as_str(), post.version))
        {
            anyhow::bail!("post template references must be unique and valid");
        }
        validate_schema(&post.schema)?;
        validate_presentation(&post.presentation, &post.schema, None)?;
        verify_post_template_hash(post)?;
    }
    verify_template_hash(instance)?;
    Ok(())
}

fn validate_snapshot_depth(value: &Value, depth: usize) -> anyhow::Result<()> {
    if depth > 16 {
        anyhow::bail!("template snapshot nesting exceeds 16 levels");
    }
    match value {
        Value::Array(items) => {
            if items.len() > 256 {
                anyhow::bail!("template arrays may contain at most 256 items");
            }
            for item in items {
                validate_snapshot_depth(item, depth + 1)?;
            }
        }
        Value::Object(map) => {
            for item in map.values() {
                validate_snapshot_depth(item, depth + 1)?;
            }
        }
        Value::String(s) if s.len() > 8192 => {
            anyhow::bail!("template strings may be at most 8192 bytes")
        }
        _ => {}
    }
    Ok(())
}

fn validate_presentation(
    presentation: &TemplatePresentation,
    schema: &TemplateSchema,
    available_post_templates: Option<&[PostTemplateSnapshot]>,
) -> anyhow::Result<()> {
    if presentation.version != 1 {
        anyhow::bail!(
            "unsupported template presentation version: {}",
            presentation.version
        );
    }
    if presentation.components.len() > 32 {
        anyhow::bail!("presentation may contain at most 32 components");
    }
    let field_ids: std::collections::HashSet<&str> = schema
        .fields
        .iter()
        .map(|field| field.id.as_str())
        .collect();
    for component in &presentation.components {
        let Some(component) = component.as_object() else {
            anyhow::bail!("presentation components must be objects");
        };
        let Some(kind) = component.get("kind").and_then(Value::as_str) else {
            anyhow::bail!("presentation components require a string kind");
        };
        if component.get("title").is_some_and(|value| {
            value
                .as_str()
                .is_none_or(|text| text.trim().is_empty() || text.len() > 128)
        }) {
            anyhow::bail!("presentation component title must be 1-128 bytes");
        }
        let require_field = |key: &str| -> anyhow::Result<()> {
            let field_id = component
                .get(key)
                .and_then(Value::as_str)
                .filter(|field_id| !field_id.trim().is_empty() && field_id.len() <= 128)
                .ok_or_else(|| anyhow::anyhow!("{kind} component requires a valid {key}"))?;
            if !field_ids.contains(field_id) {
                anyhow::bail!("{kind} component references unknown schema field {field_id:?}");
            }
            Ok(())
        };
        let validate_field_list = |key: &str, required: bool| -> anyhow::Result<Vec<&str>> {
            let Some(values) = component.get(key) else {
                if required {
                    anyhow::bail!("{kind} component requires {key}");
                }
                return Ok(Vec::new());
            };
            let values = values
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("{kind}.{key} must be an array of field IDs"))?;
            if values.len() > 256 {
                anyhow::bail!("{kind}.{key} may contain at most 256 field IDs");
            }
            let mut seen = std::collections::HashSet::new();
            let mut result = Vec::with_capacity(values.len());
            for value in values {
                let field_id = value
                    .as_str()
                    .filter(|field_id| !field_id.trim().is_empty() && field_id.len() <= 128)
                    .ok_or_else(|| {
                        anyhow::anyhow!("{kind}.{key} entries must be valid field IDs")
                    })?;
                if !field_ids.contains(field_id) {
                    anyhow::bail!("{kind} component references unknown schema field {field_id:?}");
                }
                if !seen.insert(field_id) {
                    anyhow::bail!("{kind}.{key} field IDs must be unique");
                }
                result.push(field_id);
            }
            Ok(result)
        };
        let validate_template_ids = |required: bool| -> anyhow::Result<Vec<&str>> {
            let Some(values) = component.get("post_template_ids") else {
                if required {
                    anyhow::bail!("{kind} component requires post_template_ids");
                }
                return Ok(Vec::new());
            };
            let values = values.as_array().ok_or_else(|| {
                anyhow::anyhow!("{kind}.post_template_ids must be an array of template IDs")
            })?;
            if values.len() > 100 {
                anyhow::bail!("{kind}.post_template_ids may contain at most 100 IDs");
            }
            let mut seen = std::collections::HashSet::new();
            let mut result = Vec::with_capacity(values.len());
            for value in values {
                let template_id = value
                    .as_str()
                    .filter(|template_id| {
                        !template_id.trim().is_empty() && template_id.len() <= 128
                    })
                    .ok_or_else(|| {
                        anyhow::anyhow!("{kind}.post_template_ids entries must be valid IDs")
                    })?;
                if !seen.insert(template_id) {
                    anyhow::bail!("{kind}.post_template_ids must be unique");
                }
                if available_post_templates.is_some_and(|available| {
                    !available.iter().any(|post| post.template_id == template_id)
                }) {
                    anyhow::bail!(
                        "{kind} component references unknown PostTemplate {template_id:?}"
                    );
                }
                result.push(template_id);
            }
            Ok(result)
        };
        let validate_relation = || -> anyhow::Result<()> {
            if component.get("relation").is_some_and(|value| {
                value
                    .as_str()
                    .is_none_or(|text| text.trim().is_empty() || text.len() > 64)
            }) {
                anyhow::bail!("{kind}.relation must be a 1-64 byte string when present");
            }
            Ok(())
        };
        let validate_mode = |allowed: &[&str]| -> anyhow::Result<()> {
            let mode = component
                .get("mode")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("{kind} component requires mode"))?;
            if !allowed.contains(&mode) {
                anyhow::bail!("unsupported {kind} mode {mode:?}");
            }
            Ok(())
        };

        match kind {
            "hero" => {
                require_field("title_field")?;
                validate_field_list("summary_fields", true)?;
            }
            "fields" => {
                validate_field_list("field_ids", true)?;
            }
            "related_boards" => {
                validate_relation()?;
                validate_mode(&["cards", "list"])?;
                // These IDs describe fields on linked boards, which have their own
                // independently pinned schema and are checked when rendered.
                validate_field_id_syntax_list(component.get("field_ids"), kind)?;
            }
            "related_posts" => {
                validate_relation()?;
                validate_mode(&["compact", "list"])?;
                validate_template_ids(false)?;
            }
            "post_list" | "post_timeline" => {
                validate_template_ids(false)?;
            }
            "post_groups" => {
                let template_ids = validate_template_ids(false)?;
                let group_by = component
                    .get("group_by")
                    .and_then(Value::as_str)
                    .filter(|field_id| !field_id.trim().is_empty() && field_id.len() <= 128)
                    .ok_or_else(|| {
                        anyhow::anyhow!("post_groups requires a valid group_by field ID")
                    })?;
                let selected_schemas: Vec<&TemplateSchema> = match available_post_templates {
                    Some(available) => {
                        let selected: Vec<&PostTemplateSnapshot> = available
                            .iter()
                            .filter(|post| {
                                template_ids.is_empty()
                                    || template_ids.iter().any(|id| *id == post.template_id)
                            })
                            .collect();
                        if selected.is_empty() {
                            anyhow::bail!(
                                "post_groups must select at least one pinned PostTemplate"
                            );
                        }
                        selected.iter().map(|post| &post.schema).collect()
                    }
                    // A PostTemplate presentation may group its own records. In that
                    // context its own schema is the only pinned schema available.
                    None => vec![schema],
                };
                for selected_schema in &selected_schemas {
                    if !selected_schema
                        .fields
                        .iter()
                        .any(|field| field.id == group_by)
                    {
                        anyhow::bail!("post_groups references unknown group_by field {group_by:?}");
                    }
                }
                if let Some(values) = component.get("field_ids") {
                    let fields = values.as_array().ok_or_else(|| {
                        anyhow::anyhow!("post_groups.field_ids must be an array of field IDs")
                    })?;
                    if fields.len() > 256 {
                        anyhow::bail!("post_groups.field_ids may contain at most 256 field IDs");
                    }
                    let mut seen = std::collections::HashSet::new();
                    for value in fields {
                        let field_id = value
                            .as_str()
                            .filter(|field_id| !field_id.trim().is_empty() && field_id.len() <= 128)
                            .ok_or_else(|| {
                                anyhow::anyhow!(
                                    "post_groups.field_ids entries must be valid field IDs"
                                )
                            })?;
                        if !seen.insert(field_id) {
                            anyhow::bail!("post_groups.field_ids must be unique");
                        }
                        if selected_schemas.iter().any(|selected_schema| {
                            !selected_schema
                                .fields
                                .iter()
                                .any(|field| field.id == field_id)
                        }) {
                            anyhow::bail!("post_groups references unknown post field {field_id:?}");
                        }
                    }
                }
                let groups = component.get("groups");
                let Some(groups) = groups else { continue };
                let groups = groups
                    .as_array()
                    .ok_or_else(|| anyhow::anyhow!("post_groups.groups must be an array"))?;
                if groups.len() > 100 {
                    anyhow::bail!("post_groups.groups may contain at most 100 entries");
                }
                let definitions: Vec<&TemplateField> = selected_schemas
                    .iter()
                    .map(|selected_schema| {
                        selected_schema
                            .fields
                            .iter()
                            .find(|field| field.id == group_by)
                            .expect("group_by was validated above")
                    })
                    .collect();
                let mut group_values = std::collections::HashSet::new();
                for group in groups {
                    let object = group
                        .as_object()
                        .ok_or_else(|| anyhow::anyhow!("post_groups groups must be objects"))?;
                    let value = object
                        .get("value")
                        .ok_or_else(|| anyhow::anyhow!("post_groups group requires value"))?;
                    if value.is_null()
                        || definitions
                            .iter()
                            .any(|field| !presentation_value_matches(field.field_type, value))
                    {
                        anyhow::bail!(
                            "post_groups group value has the wrong type for field {group_by:?}"
                        );
                    }
                    let value_key = serde_json::to_string(value)?;
                    if !group_values.insert(value_key) {
                        anyhow::bail!("post_groups group values must be unique");
                    }
                    if definitions.iter().any(|field| {
                        field
                            .enum_values
                            .as_ref()
                            .is_some_and(|choices| !choices.contains(value))
                    }) {
                        anyhow::bail!(
                            "post_groups group value is not declared by field {group_by:?}"
                        );
                    }
                    if definitions
                        .iter()
                        .map(|field| field.field_type)
                        .any(|field_type| field_type != definitions[0].field_type)
                    {
                        anyhow::bail!("post_groups selected templates disagree on group_by type");
                    }
                    if definitions
                        .iter()
                        .any(|field| field.enum_values != definitions[0].enum_values)
                    {
                        anyhow::bail!("post_groups selected templates disagree on group_by values");
                    }
                    let label = object
                        .get("label")
                        .and_then(Value::as_str)
                        .filter(|label| !label.trim().is_empty() && label.len() <= 128)
                        .ok_or_else(|| {
                            anyhow::anyhow!("post_groups group label must be 1-128 bytes")
                        })?;
                    let _ = label;
                }
            }
            _ => {} // Unknown component kinds round-trip for safe renderer fallback.
        }
    }
    Ok(())
}

fn validate_field_id_syntax_list(values: Option<&Value>, component: &str) -> anyhow::Result<()> {
    let Some(values) = values else { return Ok(()) };
    let values = values
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("{component}.field_ids must be an array of field IDs"))?;
    if values.len() > 256 {
        anyhow::bail!("{component}.field_ids may contain at most 256 field IDs");
    }
    let mut seen = std::collections::HashSet::new();
    for value in values {
        let field_id = value
            .as_str()
            .filter(|field_id| !field_id.trim().is_empty() && field_id.len() <= 128)
            .ok_or_else(|| {
                anyhow::anyhow!("{component}.field_ids entries must be valid field IDs")
            })?;
        if !seen.insert(field_id) {
            anyhow::bail!("{component}.field_ids must be unique");
        }
    }
    Ok(())
}

fn presentation_value_matches(field_type: TemplateFieldType, value: &Value) -> bool {
    match field_type {
        TemplateFieldType::String => value.as_str().is_some(),
        TemplateFieldType::StringList => value
            .as_array()
            .is_some_and(|items| items.iter().all(Value::is_string)),
        TemplateFieldType::Boolean => value.is_boolean(),
        TemplateFieldType::Integer => value.as_i64().is_some(),
        TemplateFieldType::Number => value.as_f64().is_some(),
    }
}

fn verify_template_hash(instance: &TemplateInstance) -> anyhow::Result<()> {
    let post_refs: Vec<Value> = instance
        .snapshot
        .post_templates
        .iter()
        .map(|post| {
            json!({
                "template_id": post.template_id,
                "version": post.version,
                "content_hash": post.content_hash,
            })
        })
        .collect();
    let definition = json!({
        "template_id": instance.template_id,
        "version": instance.version,
        "schema": instance.snapshot.schema,
        "presentation": instance.snapshot.presentation,
        "post_templates": post_refs,
    });
    verify_hash(&instance.content_hash, &definition, "board template")
}

fn verify_post_template_hash(post: &PostTemplateSnapshot) -> anyhow::Result<()> {
    let definition = json!({
        "template_id": post.template_id,
        "version": post.version,
        "schema": post.schema,
        "presentation": post.presentation,
    });
    verify_hash(&post.content_hash, &definition, "post template")
}

fn verify_hash(actual: &str, definition: &Value, label: &str) -> anyhow::Result<()> {
    let expected = content_hash(definition)?;
    if actual != expected {
        anyhow::bail!("{label} content_hash mismatch; expected {expected}");
    }
    Ok(())
}

pub fn validate_template_fields(schema: &TemplateSchema, fields: &Value) -> anyhow::Result<()> {
    validate_schema(schema)?;
    let values = fields
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("fields must be a JSON object"))?;
    if serde_json::to_vec(fields)?.len() > TEMPLATE_FIELDS_MAX_BYTES {
        anyhow::bail!("structured fields exceed 32768 bytes");
    }
    for definition in &schema.fields {
        let Some(value) = values.get(&definition.id) else {
            if definition.required {
                anyhow::bail!("missing required field {:?}", definition.id);
            }
            continue;
        };
        let valid = match definition.field_type {
            TemplateFieldType::String => value.as_str().is_some_and(|v| !v.trim().is_empty()),
            TemplateFieldType::StringList => value.as_array().is_some_and(|items| {
                items.len() <= 256
                    && items
                        .iter()
                        .all(|v| v.as_str().is_some_and(|s| !s.trim().is_empty()))
            }),
            TemplateFieldType::Boolean => value.is_boolean(),
            TemplateFieldType::Integer => value.as_i64().is_some(),
            TemplateFieldType::Number => value.as_f64().is_some(),
        };
        if !valid {
            anyhow::bail!("field {:?} has the wrong type", definition.id);
        }
        if let Some(choices) = &definition.enum_values {
            if !choices.contains(value) {
                anyhow::bail!("field {:?} must match a declared enum value", definition.id);
            }
        }
    }
    // Unknown fields are retained verbatim to permit forward-compatible round trips.
    Ok(())
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_sequence: Option<u64>,
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
    pub template_instance: Option<TemplateInstance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fields: Option<Value>,
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
    if let Some(template_instance) = &board.template_instance {
        f.insert(
            BOARD_TEMPLATE_ID_FILTER.to_string(),
            template_instance.template_id.clone(),
        );
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

/// Realistic AgentFlow-style template instances with correct JCS hashes.
#[cfg(test)]
pub(crate) mod test_fixtures {
    use super::*;

    fn jcs_hash(definition: &Value) -> String {
        use sha2::Digest;
        let bytes = serde_jcs::to_vec(definition).unwrap();
        format!("sha256:{:x}", sha2::Sha256::digest(bytes))
    }

    /// A task PostTemplate. Schema and presentation are hashed as supplied.
    pub(crate) fn task_post_template(version: u32) -> Value {
        let schema = json!({"version": 1, "fields": [
            {"id": "description", "type": "string", "required": true, "label": "Description"},
            {"id": "status", "type": "string", "required": true,
             "enum": ["ready", "active", "blocked", "review", "done"]},
            {"id": "estimate", "type": "integer", "required": false},
            {"id": "owner", "type": "string", "required": false, "x_hint": "assignee"}
        ]});
        let presentation = json!({"version": 1, "components": [
            {"kind": "fields", "field_ids": ["description", "status"]}
        ]});
        let hash = jcs_hash(&json!({
            "template_id": "task", "version": version,
            "schema": schema, "presentation": presentation,
        }));
        json!({"template_id": "task", "version": version, "content_hash": hash,
               "schema": schema, "presentation": presentation})
    }

    /// A project BoardTemplate instance pinning one task PostTemplate. The
    /// caller may add presentation components; hashes are recomputed.
    pub(crate) fn project_instance_with(version: u32, extra_components: Vec<Value>) -> Value {
        let schema = json!({"version": 1, "fields": [
            {"id": "goal", "type": "string", "required": true, "label": "Goal"},
            {"id": "success_criteria", "type": "string_list", "required": true},
            {"id": "coordinator", "type": "string", "required": false}
        ]});
        let mut components = vec![
            json!({"kind": "hero", "title_field": "goal", "summary_fields": ["success_criteria"]}),
            json!({"kind": "post_groups", "post_template_ids": ["task"], "group_by": "status",
                   "groups": [{"value": "active", "label": "Active"},
                              {"value": "review", "label": "Review"}]}),
            // Unknown kind: must round-trip for renderer fallback.
            json!({"kind": "kanban_future", "lanes": 3}),
        ];
        components.extend(extra_components);
        let presentation = json!({"version": 1, "components": components});
        let task = task_post_template(1);
        let refs = json!([{"template_id": "task", "version": 1,
                           "content_hash": task["content_hash"]}]);
        let hash = jcs_hash(&json!({
            "template_id": "project", "version": version,
            "schema": schema, "presentation": presentation, "post_templates": refs,
        }));
        json!({"contract_version": 1, "issuer": "agentflow", "template_id": "project",
               "version": version, "content_hash": hash,
               "snapshot": {"schema": schema, "presentation": presentation,
                            "post_templates": [task]}})
    }

    pub(crate) fn project_instance(version: u32) -> TemplateInstance {
        serde_json::from_value(project_instance_with(version, Vec::new())).unwrap()
    }

    pub(crate) fn project_fields() -> Value {
        json!({"goal": "Ship template boards", "success_criteria": ["Tests pass"],
               "unknown_note": {"kept": true}})
    }
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

    fn golden_post_template() -> PostTemplateSnapshot {
        serde_json::from_value(json!({
            "template_id": "task",
            "version": 1,
            // Computed independently with Python's json.dumps(sort_keys=True,
            // separators=(",", ":")) + SHA-256, which equals RFC 8785 JCS
            // for this ASCII/integer-only definition.
            "content_hash": "sha256:b1045979bf45136e840b67871023d61cb2a71757b21bf7d0c8f4a52848b63464",
            "schema": {"version": 1, "fields": [
                {"id": "status", "type": "string", "required": true, "enum": ["ready", "done"]}
            ]},
            "presentation": {"version": 1, "components": [
                {"kind": "fields", "field_ids": ["status"]}
            ]}
        }))
        .unwrap()
    }

    #[test]
    fn post_template_hash_matches_independent_jcs_vector() {
        verify_post_template_hash(&golden_post_template()).unwrap();
        let mut tampered = golden_post_template();
        tampered.schema.fields[0].required = false;
        let err = verify_post_template_hash(&tampered).unwrap_err();
        assert!(err.to_string().contains("content_hash mismatch"), "{err}");
    }

    #[test]
    fn unknown_field_type_is_rejected_at_parse() {
        let err = serde_json::from_value::<TemplateField>(
            json!({"id": "due", "type": "date", "required": false}),
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown variant"), "{err}");
    }

    #[test]
    fn unknown_field_metadata_round_trips() {
        let field: TemplateField = serde_json::from_value(
            json!({"id": "x", "type": "string", "required": false, "x_hint": {"a": 1}}),
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(&field).unwrap()["x_hint"],
            json!({"a": 1})
        );
    }

    #[test]
    fn legacy_board_and_post_records_deserialize_without_template_data() {
        // Pre-typed record: no type, links, template_instance, or fields.
        let board = board_from_doc_content(
            r#"{"board_id":"board:u:ws:k","owner":"u","workspace":"ws","key":"k","title":"K","created_at":"2026-01-01T00:00:00Z"}"#,
        )
        .expect("legacy board parses");
        assert_eq!(board.board_type, DEFAULT_BOARD_TYPE);
        assert!(board.links.is_empty());
        assert!(board.template_instance.is_none() && board.fields.is_none());
        // Typed-board prototype record carrying the retired built-in refs.
        let prototype = board_from_doc_content(
            r#"{"board_id":"board:u:ws:p","owner":"u","workspace":"ws","key":"p","title":"P","type":"project","links":[],"template_id":"project","template_version":1,"created_at":"2026-01-01T00:00:00Z"}"#,
        )
        .expect("prototype board parses");
        assert_eq!(prototype.board_type, "project");
        assert!(prototype.template_instance.is_none());
        let post: BoardPost = serde_json::from_str(
            r#"{"post_id":"p1","board_id":"board:u:ws:k","author_agent_id":"a","provider":"codex","content":"hi","sequence":1,"created_at":"t"}"#,
        )
        .unwrap();
        assert!(post.fields.is_none() && post.post_template_id.is_none());
        assert!(post.links.is_empty());
    }

    #[test]
    fn template_fields_validate_known_and_keep_unknown() {
        let schema = golden_post_template().schema;
        validate_template_fields(&schema, &json!({"status": "done", "future": [1, 2]})).unwrap();
        for (fields, needle) in [
            (json!({}), "missing required field"),
            (json!({"status": 3}), "wrong type"),
            (json!({"status": "  "}), "wrong type"),
            (json!({"status": "active"}), "declared enum"),
            (json!(["status"]), "JSON object"),
        ] {
            let err = validate_template_fields(&schema, &fields).unwrap_err();
            assert!(err.to_string().contains(needle), "{fields}: {err}");
        }
    }
}
