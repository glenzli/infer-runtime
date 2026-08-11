//! Normalization for the single hosted Web Search tool exposed by the Codex
//! App Server bridge. This module owns only completed search evidence; session
//! and turn lifecycle remain in the parent adapter.

use serde_json::{Map, Value, json};

use crate::ProviderError;

#[derive(Debug, Clone, PartialEq)]
pub(super) struct CompletedWebSearch {
    id: String,
    action: Value,
}

impl CompletedWebSearch {
    pub(super) fn from_completed_item(item: &Value) -> Result<Self, ProviderError> {
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| ProviderError::Protocol("webSearch item omitted id".into()))?
            .to_owned();
        let query = item
            .get("query")
            .and_then(Value::as_str)
            .filter(|query| !query.is_empty())
            .map(str::to_owned);
        let action = normalize_action(item.get("action"), query)?;
        Ok(Self { id, action })
    }

    pub(super) fn id(&self) -> &str {
        &self.id
    }

    pub(super) fn response_item(&self) -> Value {
        json!({
            "id": self.id,
            "type": "web_search_call",
            "status": "completed",
            "action": self.action,
        })
    }
}

fn normalize_action(
    action: Option<&Value>,
    fallback_query: Option<String>,
) -> Result<Value, ProviderError> {
    let Some(action) = action else {
        return fallback_query
            .map(|query| json!({"type": "search", "query": query}))
            .ok_or_else(|| {
                ProviderError::Protocol("webSearch item omitted action and query".into())
            });
    };
    let object = action
        .as_object()
        .ok_or_else(|| ProviderError::Protocol("webSearch action was not an object".into()))?;
    let action_type = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| ProviderError::Protocol("webSearch action omitted type".into()))?;
    let mut normalized = Map::new();
    match action_type {
        "search" => {
            normalized.insert("type".into(), Value::String("search".into()));
            copy_optional_string(object, &mut normalized, "query", "query")?;
            copy_optional_string_array(object, &mut normalized, "queries", "queries")?;
            if !normalized.contains_key("query") && !normalized.contains_key("queries") {
                let query = fallback_query.ok_or_else(|| {
                    ProviderError::Protocol("webSearch search action omitted its query".into())
                })?;
                normalized.insert("query".into(), Value::String(query));
            }
        }
        "openPage" => {
            normalized.insert("type".into(), Value::String("open_page".into()));
            copy_required_string(object, &mut normalized, "url", "url")?;
        }
        "findInPage" => {
            normalized.insert("type".into(), Value::String("find_in_page".into()));
            copy_required_string(object, &mut normalized, "url", "url")?;
            copy_required_string(object, &mut normalized, "pattern", "pattern")?;
        }
        _ => {
            return Err(ProviderError::Protocol(
                "webSearch action used an unknown type".into(),
            ));
        }
    }
    Ok(Value::Object(normalized))
}

fn copy_required_string(
    source: &Map<String, Value>,
    target: &mut Map<String, Value>,
    source_key: &str,
    target_key: &str,
) -> Result<(), ProviderError> {
    let value = source
        .get(source_key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ProviderError::Protocol(format!("webSearch action omitted {source_key}")))?;
    target.insert(target_key.into(), Value::String(value.into()));
    Ok(())
}

fn copy_optional_string(
    source: &Map<String, Value>,
    target: &mut Map<String, Value>,
    source_key: &str,
    target_key: &str,
) -> Result<(), ProviderError> {
    if source.contains_key(source_key) {
        copy_required_string(source, target, source_key, target_key)?;
    }
    Ok(())
}

fn copy_optional_string_array(
    source: &Map<String, Value>,
    target: &mut Map<String, Value>,
    source_key: &str,
    target_key: &str,
) -> Result<(), ProviderError> {
    let Some(value) = source.get(source_key) else {
        return Ok(());
    };
    let values = value
        .as_array()
        .filter(|values| !values.is_empty())
        .ok_or_else(|| {
            ProviderError::Protocol(format!("webSearch action {source_key} was invalid"))
        })?;
    if values
        .iter()
        .any(|value| value.as_str().is_none_or(str::is_empty))
    {
        return Err(ProviderError::Protocol(format!(
            "webSearch action {source_key} was invalid"
        )));
    }
    target.insert(target_key.into(), value.clone());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_app_server_action_names_to_responses_items() {
        let search = CompletedWebSearch::from_completed_item(&json!({
            "id": "ws-1",
            "type": "webSearch",
            "query": "runtime news",
            "action": {"type": "search", "queries": ["runtime news", "runtime release"]}
        }))
        .unwrap();
        assert_eq!(search.response_item()["type"], "web_search_call");
        assert_eq!(search.response_item()["action"]["type"], "search");
        assert_eq!(
            search.response_item()["action"]["queries"][1],
            "runtime release"
        );

        let open = CompletedWebSearch::from_completed_item(&json!({
            "id": "ws-2",
            "type": "webSearch",
            "query": "ignored",
            "action": {"type": "openPage", "url": "https://example.com/source"}
        }))
        .unwrap();
        assert_eq!(open.response_item()["action"]["type"], "open_page");
    }

    #[test]
    fn malformed_or_unknown_actions_fail_closed() {
        assert!(
            CompletedWebSearch::from_completed_item(&json!({
                "id": "ws-1", "type": "webSearch", "action": {"type": "command"}
            }))
            .is_err()
        );
        assert!(
            CompletedWebSearch::from_completed_item(&json!({
                "id": "ws-1", "type": "webSearch", "action": {"type": "openPage"}
            }))
            .is_err()
        );
    }
}
