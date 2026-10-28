//! Typed subset of Mem0 Platform's nested metadata filter expression.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Mem0FilterExpr {
    And {
        children: Vec<Mem0FilterExpr>,
    },
    Or {
        children: Vec<Mem0FilterExpr>,
    },
    Not {
        child: Box<Mem0FilterExpr>,
    },
    Compare {
        field: String,
        operator: Mem0FilterOperator,
        value: Value,
    },
    Exists {
        field: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mem0FilterOperator {
    Eq,
    Ne,
    In,
    Gt,
    Gte,
    Lt,
    Lte,
    Contains,
    Icontains,
}

impl Mem0FilterExpr {
    /// Parse the Platform's JSON shape into a validated typed expression.
    pub fn from_platform_value(value: Value) -> anyhow::Result<Self> {
        parse_expression(value, 0)
    }

    pub(crate) fn matches(&self, document: &crate::SourceDocument) -> bool {
        match self {
            Self::And { children } => children.iter().all(|child| child.matches(document)),
            Self::Or { children } => children.iter().any(|child| child.matches(document)),
            Self::Not { child } => !child.matches(document),
            Self::Exists { field } => {
                if !filterable_field(field) {
                    return false;
                }
                field_value(document, field).is_some_and(|value| !value.is_null())
            }
            Self::Compare {
                field,
                operator,
                value,
            } => {
                if !filterable_field(field) {
                    return false;
                }
                if field == "metadata" {
                    return compare_metadata(document, *operator, value);
                }
                let Some(actual) = field_value(document, field) else {
                    return matches!(operator, Mem0FilterOperator::Ne);
                };
                compare(&actual, *operator, value)
            }
        }
    }

    pub(crate) fn validate_mem0_fields(&self) -> anyhow::Result<()> {
        self.validate_depth(0)
    }

    fn validate_depth(&self, depth: usize) -> anyhow::Result<()> {
        if depth > 24 {
            anyhow::bail!("Mem0 filters may nest at most 24 levels");
        }
        match self {
            Self::And { children } | Self::Or { children } => {
                if children.is_empty() {
                    anyhow::bail!("AND/OR arrays must not be empty");
                }
                for child in children {
                    child.validate_depth(depth + 1)?;
                }
            }
            Self::Not { child } => child.validate_depth(depth + 1)?,
            Self::Exists { field } => validate_field(field)?,
            Self::Compare {
                field,
                operator,
                value,
            } => {
                validate_field(field)?;
                if field == "user_id"
                    && (*operator != Mem0FilterOperator::Eq
                        || value
                            .as_str()
                            .is_none_or(|user_id| user_id.trim().is_empty()))
                {
                    anyhow::bail!(
                        "Mem0 owner scope requires a positive nonempty user_id equality filter"
                    );
                }
                if *operator == Mem0FilterOperator::In && !value.is_array() {
                    anyhow::bail!("'in' filter for '{field}' requires an array");
                }
                if field == "metadata"
                    && !matches!(
                        operator,
                        Mem0FilterOperator::Eq
                            | Mem0FilterOperator::Ne
                            | Mem0FilterOperator::Contains
                    )
                {
                    anyhow::bail!("metadata object filters support eq, ne, and contains");
                }
                if field == "metadata" && !value.is_object() {
                    anyhow::bail!("metadata object filters require a JSON object");
                }
                if field == "metadata" {
                    if let Some(object) = value.as_object() {
                        for metadata_key in object.keys() {
                            if !crate::memory_api::is_user_metadata_filter(metadata_key) {
                                anyhow::bail!("Mem0 metadata filters cannot access internal field '{metadata_key}'");
                            }
                        }
                    }
                }
                if *operator == Mem0FilterOperator::Icontains
                    && field != "user_id"
                    && field != "run_id"
                {
                    anyhow::bail!("Mem0 metadata filters do not support 'icontains'");
                }
            }
        }
        Ok(())
    }
}

fn validate_field(field: &str) -> anyhow::Result<()> {
    if matches!(
        field,
        "agent_id"
            | "app_id"
            | "categories"
            | "created_at"
            | "updated_at"
            | "timestamp"
            | "expiration_date"
            | "keywords"
            | "memory_ids"
    ) {
        anyhow::bail!("Mem0Adapter filters do not support the '{field}' field");
    }
    if matches!(
        field,
        "memory_user_id"
            | "request_id"
            | "request_fingerprint"
            | "expires_at_ms"
            | "supersedes_id"
            | "session_id"
    ) {
        anyhow::bail!("Mem0 filters cannot access Membrick internal field '{field}'");
    }
    if field != "user_id" && field != "run_id" && field != "metadata" && field.starts_with('$') {
        anyhow::bail!("invalid Mem0 filter field '{field}'");
    }
    if !filterable_field(field) {
        anyhow::bail!("Mem0 filters cannot access internal or unsupported field '{field}'");
    }
    Ok(())
}

fn filterable_field(field: &str) -> bool {
    matches!(field, "user_id" | "run_id" | "metadata")
        || crate::memory_api::is_user_metadata_filter(field)
}

impl Mem0FilterExpr {
    pub(crate) fn user_ids(&self, output: &mut Vec<String>) {
        match self {
            // Only an equality required by the whole expression can provide
            // the adapter's ownership context. An OR branch is optional.
            Self::And { children } => {
                for child in children {
                    child.user_ids(output);
                }
            }
            // OR and NOT predicates do not establish a positive owner scope.
            Self::Or { .. } | Self::Not { .. } => {}
            Self::Compare {
                field,
                operator: Mem0FilterOperator::Eq,
                value,
            } if field == "user_id" => {
                if let Some(user_id) = value.as_str() {
                    output.push(user_id.to_owned());
                }
            }
            _ => {}
        }
    }

    pub(crate) fn run_ids(&self, output: &mut Vec<String>) {
        match self {
            // A run predicate under AND is required to match. OR branches are
            // optional, and negated predicates identify runs that must not
            // match; neither case establishes a requested run attribution.
            Self::And { children } => {
                for child in children {
                    child.run_ids(output);
                }
            }
            Self::Or { .. } | Self::Not { .. } => {}
            Self::Compare {
                field,
                operator: Mem0FilterOperator::Eq,
                value,
            } if field == "run_id" => {
                if let Some(run_id) = value.as_str() {
                    output.push(run_id.to_owned());
                }
            }
            _ => {}
        }
    }
}

fn parse_expression(value: Value, depth: usize) -> anyhow::Result<Mem0FilterExpr> {
    if depth > 24 {
        anyhow::bail!("Mem0 filters may nest at most 24 levels");
    }
    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("Mem0 filters must be JSON objects"))?;
    if object.is_empty() {
        anyhow::bail!("Mem0 filter objects must not be empty");
    }
    let mut children = Vec::with_capacity(object.len());
    for (field, condition) in object {
        if field == "AND" || field == "OR" {
            let items = condition
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("{field} requires an array"))?;
            if items.is_empty() {
                anyhow::bail!("AND/OR arrays must not be empty");
            }
            let parsed = items
                .iter()
                .cloned()
                .map(|v| parse_expression(v, depth + 1))
                .collect::<anyhow::Result<Vec<_>>>()?;
            children.push(if field == "AND" {
                Mem0FilterExpr::And { children: parsed }
            } else {
                Mem0FilterExpr::Or { children: parsed }
            });
        } else if field == "NOT" {
            let child = if let Some(items) = condition.as_array() {
                if items.len() != 1 {
                    anyhow::bail!("NOT requires one filter expression");
                }
                items[0].clone()
            } else {
                condition.clone()
            };
            children.push(Mem0FilterExpr::Not {
                child: Box::new(parse_expression(child, depth + 1)?),
            });
        } else {
            if field.starts_with('$') {
                anyhow::bail!("invalid Mem0 filter expression key '{field}'");
            }
            children.extend(parse_condition(field, condition.clone())?);
        }
    }
    if children.len() == 1 {
        Ok(children.remove(0))
    } else {
        Ok(Mem0FilterExpr::And { children })
    }
}

fn parse_condition(field: &str, value: Value) -> anyhow::Result<Vec<Mem0FilterExpr>> {
    if value == Value::String("*".to_owned()) {
        return Ok(vec![Mem0FilterExpr::Exists {
            field: field.to_owned(),
        }]);
    }
    if let Some(object) = value.as_object() {
        let known = [
            "eq",
            "ne",
            "in",
            "gt",
            "gte",
            "lt",
            "lte",
            "contains",
            "icontains",
        ];
        let ops = object
            .keys()
            .filter(|key| known.contains(&key.as_str()))
            .collect::<Vec<_>>();
        if !ops.is_empty() {
            if ops.len() != object.len() {
                anyhow::bail!("filter condition for '{field}' mixes operators with ordinary keys");
            }
            return ops
                .into_iter()
                .map(|key| {
                    let operator = match key.as_str() {
                        "eq" => Mem0FilterOperator::Eq,
                        "ne" => Mem0FilterOperator::Ne,
                        "in" => Mem0FilterOperator::In,
                        "gt" => Mem0FilterOperator::Gt,
                        "gte" => Mem0FilterOperator::Gte,
                        "lt" => Mem0FilterOperator::Lt,
                        "lte" => Mem0FilterOperator::Lte,
                        "contains" => Mem0FilterOperator::Contains,
                        "icontains" => Mem0FilterOperator::Icontains,
                        _ => unreachable!(),
                    };
                    let operand = object.get(key).expect("key from object").clone();
                    if operator == Mem0FilterOperator::In && !operand.is_array() {
                        anyhow::bail!("'in' filter for '{field}' requires an array");
                    }
                    Ok(Mem0FilterExpr::Compare {
                        field: field.to_owned(),
                        operator,
                        value: operand,
                    })
                })
                .collect();
        }
        if let Some(operator) = object.keys().find(|key| looks_like_operator(key)) {
            anyhow::bail!("unsupported Mem0 filter operator '{operator}' for '{field}'");
        }
    }
    Ok(vec![Mem0FilterExpr::Compare {
        field: field.to_owned(),
        operator: Mem0FilterOperator::Eq,
        value,
    }])
}

fn looks_like_operator(key: &str) -> bool {
    const OPERATORS: [&str; 9] = [
        "eq",
        "ne",
        "in",
        "gt",
        "gte",
        "lt",
        "lte",
        "contains",
        "icontains",
    ];
    if key.starts_with('$') || key.chars().any(|c| matches!(c, '<' | '>' | '=')) {
        return true;
    }
    let candidate = key.to_ascii_lowercase();
    OPERATORS.iter().any(|operator| {
        candidate.len() >= 3
            && operator.len() >= 3
            && edit_distance_at_most_two(&candidate, operator)
    })
}

fn edit_distance_at_most_two(left: &str, right: &str) -> bool {
    let left = left.chars().collect::<Vec<_>>();
    let right = right.chars().collect::<Vec<_>>();
    if left.len().abs_diff(right.len()) > 2 {
        return false;
    }
    let mut previous = (0..=right.len()).collect::<Vec<_>>();
    let mut current = vec![0; right.len() + 1];
    for (i, left_char) in left.iter().enumerate() {
        current[0] = i + 1;
        for (j, right_char) in right.iter().enumerate() {
            current[j + 1] = (previous[j + 1] + 1)
                .min(current[j] + 1)
                .min(previous[j] + usize::from(left_char != right_char));
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()] <= 2
}

fn field_value(document: &crate::SourceDocument, field: &str) -> Option<Value> {
    match field {
        "user_id" => document
            .filters
            .get("memory_user_id")
            .cloned()
            .map(Value::String),
        "run_id" => document
            .group_id
            .as_deref()
            .map(crate::provider_sources::session_group)
            .map(|s| Value::String(s.to_owned())),
        "metadata" => Some(Value::Object(metadata_value(document))),
        key if crate::memory_api::is_user_metadata_filter(key) => document
            .filters
            .get(key)
            .filter(|value| value.starts_with("mem0-json:"))
            .map(|value| decode_metadata(value)),
        _ => None,
    }
}

fn metadata_value(document: &crate::SourceDocument) -> serde_json::Map<String, Value> {
    document
        .filters
        .iter()
        .filter(|(key, value)| {
            crate::memory_api::is_user_metadata_filter(key) && value.starts_with("mem0-json:")
        })
        .map(|(key, value)| (key.clone(), decode_metadata(value)))
        .collect()
}

fn compare_metadata(
    document: &crate::SourceDocument,
    operator: Mem0FilterOperator,
    expected: &Value,
) -> bool {
    let Some(expected) = expected.as_object() else {
        return false;
    };
    let actual = metadata_value(document);
    let all_equal = expected
        .iter()
        .all(|(key, value)| actual.get(key) == Some(value));
    match operator {
        Mem0FilterOperator::Eq | Mem0FilterOperator::Contains => all_equal,
        Mem0FilterOperator::Ne => !all_equal,
        _ => false,
    }
}

fn decode_metadata(value: &str) -> Value {
    value
        .strip_prefix("mem0-json:")
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_else(|| Value::String(value.to_owned()))
}

fn compare(actual: &Value, operator: Mem0FilterOperator, expected: &Value) -> bool {
    match operator {
        Mem0FilterOperator::Eq => actual == expected,
        Mem0FilterOperator::Ne => actual != expected,
        Mem0FilterOperator::In => expected
            .as_array()
            .is_some_and(|values| values.iter().any(|v| actual == v)),
        Mem0FilterOperator::Gt => ordered(actual, expected).is_some_and(|o| o.is_gt()),
        Mem0FilterOperator::Gte => ordered(actual, expected).is_some_and(|o| !o.is_lt()),
        Mem0FilterOperator::Lt => ordered(actual, expected).is_some_and(|o| o.is_lt()),
        Mem0FilterOperator::Lte => ordered(actual, expected).is_some_and(|o| !o.is_gt()),
        Mem0FilterOperator::Contains => {
            actual
                .as_array()
                .is_some_and(|values| values.contains(expected))
                || actual == expected
        }
        Mem0FilterOperator::Icontains => actual
            .as_str()
            .zip(expected.as_str())
            .is_some_and(|(a, e)| a.to_lowercase().contains(&e.to_lowercase())),
    }
}

fn ordered(left: &Value, right: &Value) -> Option<std::cmp::Ordering> {
    if let (Some(a), Some(b)) = (left.as_u64(), right.as_u64()) {
        return Some(a.cmp(&b));
    }
    if let (Some(a), Some(b)) = (left.as_i64(), right.as_i64()) {
        return Some(a.cmp(&b));
    }
    if let (Some(a), Some(b)) = (left.as_i64(), right.as_u64()) {
        return Some(if a < 0 {
            std::cmp::Ordering::Less
        } else {
            (a as u64).cmp(&b)
        });
    }
    if let (Some(a), Some(b)) = (left.as_u64(), right.as_i64()) {
        return Some(if b < 0 {
            std::cmp::Ordering::Greater
        } else {
            a.cmp(&(b as u64))
        });
    }
    if let (Some(a), Some(b)) = (left.as_f64(), right.as_f64()) {
        return a.partial_cmp(&b);
    }
    left.as_str().zip(right.as_str()).map(|(a, b)| a.cmp(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn doc(metadata: &[(&str, Value)]) -> crate::SourceDocument {
        crate::SourceDocument {
            doc_id: "doc".into(),
            source: "memory://doc".into(),
            content: "test".into(),
            concept: String::new(),
            group_id: Some("run-1".into()),
            headings: vec![],
            links: vec![],
            timestamp: None,
            doc_length: 1,
            author_agent: Some("user".into()),
            filters: {
                let mut filters = BTreeMap::from([("memory_user_id".into(), "alice".into())]);
                filters.extend(
                    metadata
                        .iter()
                        .map(|(key, value)| (key.to_string(), format!("mem0-json:{value}"))),
                );
                filters
            },
            key_phrases: vec![],
            key_phrase_extraction_hash: String::new(),
        }
    }

    #[test]
    fn nested_boolean_and_json_comparison_filters_match() {
        let document = doc(&[
            ("age", serde_json::json!(34)),
            ("tags", serde_json::json!(["work", "rust"])),
        ]);
        let filter = Mem0FilterExpr::from_platform_value(serde_json::json!({"AND":[
            {"user_id":"alice"}, {"OR":[{"age":{"gte":30}},{"tags":{"contains":"rust"}}]},
            {"NOT":{"age":{"lt":18}}}
        ]}))
        .unwrap();
        assert!(filter.matches(&document));
    }

    #[test]
    fn metadata_object_filter_matches_requested_pairs_without_requiring_exact_map() {
        let document = doc(&[
            ("source", serde_json::json!("import")),
            ("priority", serde_json::json!(4)),
        ]);
        let filter = Mem0FilterExpr::from_platform_value(
            serde_json::json!({"metadata":{"source":"import"}}),
        )
        .unwrap();
        assert!(filter.matches(&document));
    }

    #[test]
    fn comparison_operators_handle_json_values_and_missing_fields() {
        let document = doc(&[
            ("priority", serde_json::json!(4)),
            ("tags", serde_json::json!(["rust"])),
        ]);
        for value in [
            serde_json::json!({"priority":{"in":[3,4]}}),
            serde_json::json!({"priority":{"gt":3}}),
            serde_json::json!({"tags":{"contains":"rust"}}),
            serde_json::json!({"missing":{"ne":"value"}}),
        ] {
            assert!(Mem0FilterExpr::from_platform_value(value)
                .unwrap()
                .matches(&document));
        }
        assert!(!Mem0FilterExpr::from_platform_value(
            serde_json::json!({"missing":{"eq":"value"}}),
        )
        .unwrap()
        .matches(&document));
    }

    #[test]
    fn rejects_empty_or_malformed_logical_expressions() {
        for value in [
            serde_json::json!({"AND": []}),
            serde_json::json!({"OR": []}),
            serde_json::json!({"NOT": []}),
            serde_json::json!({"priority":{"in":"high"}}),
            serde_json::json!({"priority":{"gtee":3}}),
            serde_json::json!({"priority":{"$gte":3}}),
        ] {
            assert!(Mem0FilterExpr::from_platform_value(value).is_err());
        }
    }
}
