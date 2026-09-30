use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};
use stravia_vendor_common::common;
use stravia_vendor_sdk::{
    ConfigValidationResponse, DiscoverRequest, DiscoverResponse, DiscoveredModel, ErrorKind,
    GuestHost, HttpRequest, PluginError, ProviderSnapshot, ValidationIssue, read_http_body,
};

use crate::{Region, auth, messages};

const MAX_CONFIG_BODY: usize = 8 * 1024 * 1024;

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 200
        && !id.chars().any(char::is_whitespace)
        && !id.chars().any(char::is_control)
}

fn configured_ids(options: &BTreeMap<String, Value>) -> Result<Vec<&str>, ()> {
    let Some(value) = options.get("model_ids") else {
        return Ok(Vec::new());
    };
    let text = value.as_str().ok_or(())?;
    if text.len() > 16384 {
        return Err(());
    }
    let mut seen = BTreeSet::new();
    let mut ids = Vec::new();
    for id in text.lines().map(str::trim).filter(|id| !id.is_empty()) {
        if !valid_id(id) || !seen.insert(id) {
            return Err(());
        }
        ids.push(id);
    }
    Ok(ids)
}

pub(crate) fn validate(options: &BTreeMap<String, Value>) -> ConfigValidationResponse {
    let mut issues = Vec::new();
    if options
        .keys()
        .any(|key| key != "model_ids" && key != "auto_paid_on_rate_limit")
    {
        issues.push(ValidationIssue {
            field: None,
            code: "unknown_option".into(),
            message: messages::invalid_options(),
        });
    }
    if options
        .get("auto_paid_on_rate_limit")
        .is_some_and(|value| !value.is_boolean())
    {
        issues.push(ValidationIssue {
            field: Some("auto_paid_on_rate_limit".into()),
            code: "invalid_auto_paid_on_rate_limit".into(),
            message: messages::invalid_auto_paid_on_rate_limit(),
        });
    }
    if configured_ids(options).is_err() {
        issues.push(ValidationIssue {
            field: Some("model_ids".into()),
            code: "invalid_model_ids".into(),
            message: messages::invalid_model_ids(),
        });
    }
    ConfigValidationResponse {
        issues,
        proposed_base_url: None,
    }
}

pub(crate) fn discover(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    region: Region,
    request: DiscoverRequest,
) -> Result<DiscoverResponse, PluginError> {
    if request.cursor.is_some() {
        return Err(common::plugin_error(
            ErrorKind::Invalid,
            "WorkBuddy model discovery is not paginated",
        ));
    }
    let headers = auth::headers(provider, region)?;
    if !validate(&provider.options).issues.is_empty() {
        return Err(common::plugin_error(
            ErrorKind::Invalid,
            "invalid WorkBuddy model options",
        ));
    }
    if let Some(rows) = provider.operation_metadata.get("static_models") {
        let rows = rows.as_array().ok_or_else(|| {
            common::plugin_error(ErrorKind::Invalid, "static_models must be an array")
        })?;
        return parse_models(rows, "administrator");
    }
    let ids = configured_ids(&provider.options)
        .map_err(|()| common::plugin_error(ErrorKind::Invalid, "invalid model IDs"))?;
    if !ids.is_empty() {
        let rows: Vec<Value> = ids.into_iter().map(|id| json!({"id": id})).collect();
        return parse_models(&rows, "administrator");
    }
    let config = load_config(host, region, headers)?;
    client_catalog(&config)
}

fn load_config(
    host: &GuestHost,
    region: Region,
    headers: Vec<(String, String)>,
) -> Result<Value, PluginError> {
    let response = host.http_start(HttpRequest {
        method: "GET".into(),
        url: format!("{}/v3/config", region.origin()),
        headers,
        body: Vec::new(),
    })?;
    let status = response.status()?;
    let response_headers = response.headers()?;
    let body = read_http_body(&response, MAX_CONFIG_BODY)?;
    if !(200..300).contains(&status) {
        let mut error = common::upstream_error(status, &response_headers, &body);
        error.message = format!("WorkBuddy model discovery HTTP {status}");
        return Err(error);
    }
    let mut payload: Value = serde_json::from_slice(&body).map_err(|_| {
        common::plugin_error(
            ErrorKind::upstream_unknown(),
            "invalid WorkBuddy model configuration JSON",
        )
    })?;
    if payload
        .get("code")
        .is_some_and(|code| code.as_i64() != Some(0))
    {
        return Err(common::plugin_error(
            ErrorKind::upstream_unknown(),
            "WorkBuddy model configuration returned an error",
        ));
    }
    if let Some(data) = payload
        .as_object_mut()
        .and_then(|object| object.remove("data"))
    {
        Ok(data)
    } else {
        Ok(payload)
    }
}

fn catalog_rows(config: &Value) -> Result<&[Value], PluginError> {
    config
        .get("models")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| {
            common::plugin_error(
                ErrorKind::upstream_unknown(),
                "WorkBuddy configuration has no models array",
            )
        })
}

fn paid_lines(config: &Value) -> Result<BTreeMap<String, Value>, PluginError> {
    let rows = catalog_rows(config)?;
    let Some(lines) = config
        .pointer("/productFeaturesConfig/ModelRateLimitCap/lines")
        .and_then(Value::as_array)
    else {
        return Ok(BTreeMap::new());
    };
    let mut pairs = BTreeMap::new();
    let mut used_ids = BTreeSet::new();
    for line in lines {
        let (Some(free), Some(paid)) = (
            line.get("freeId").and_then(Value::as_str),
            line.get("paidId").and_then(Value::as_str),
        ) else {
            continue;
        };
        let allow = match line.get("allowPaidSwitch") {
            None => false,
            Some(Value::Bool(allow)) => *allow,
            Some(_) => continue,
        };
        if free == paid || !valid_id(free) || !valid_id(paid) {
            continue;
        }
        let find = |id: &str| {
            rows.iter()
                .find(|row| row.get("id").and_then(Value::as_str) == Some(id))
        };
        let (Some(_), Some(paid_row)) = (find(free), find(paid)) else {
            continue;
        };
        // 配对冲突时无法明确授权付费目的地，必须拒绝而不是猜测。
        if used_ids.contains(free) || used_ids.contains(paid) {
            return Err(common::plugin_error(
                ErrorKind::upstream_unknown(),
                "ambiguous WorkBuddy model rate-limit lines",
            ));
        }
        used_ids.insert(free);
        used_ids.insert(paid);
        let paid_model = parse_models(std::slice::from_ref(paid_row), "account-config")?
            .models
            .remove(0);
        pairs.insert(
            free.into(),
            json!({
                "freeId": free, "paidId": paid, "allowPaidSwitch": allow,
                "displayName": line.get("displayName").and_then(Value::as_str),
                "paidMetadata": paid_model.metadata,
            }),
        );
    }
    Ok(pairs)
}

pub(crate) fn paid_line(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    region: Region,
    free_id: &str,
) -> Result<Option<Value>, PluginError> {
    let config = load_config(host, region, auth::headers(provider, region)?)?;
    Ok(paid_lines(&config)?.remove(free_id))
}

fn client_catalog(config: &Value) -> Result<DiscoverResponse, PluginError> {
    let rows = catalog_rows(config)?;
    let agents = config
        .get("agents")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            common::plugin_error(
                ErrorKind::upstream_unknown(),
                "WorkBuddy configuration has no client model list",
            )
        })?;
    let nonempty = |agent: &&Value| {
        agent
            .get("models")
            .and_then(Value::as_array)
            .is_some_and(|models| !models.is_empty())
    };
    let agent = agents
        .iter()
        .find(|agent| {
            agent
                .get("tags")
                .and_then(Value::as_array)
                .is_some_and(|tags| tags.iter().any(|tag| tag.as_str() == Some("default")))
        })
        .or_else(|| {
            agents
                .iter()
                .find(|agent| agent.get("name").and_then(Value::as_str) == Some("cli"))
        })
        .or_else(|| agents.iter().find(nonempty))
        .ok_or_else(|| {
            common::plugin_error(
                ErrorKind::upstream_unknown(),
                "WorkBuddy configuration has no client model list",
            )
        })?;
    let pairs = paid_lines(config)?;
    let mut selected = Vec::new();
    let mut seen = BTreeSet::new();
    let agent_models = agent
        .get("models")
        .and_then(Value::as_array)
        .filter(|models| !models.is_empty())
        .ok_or_else(|| {
            common::plugin_error(
                ErrorKind::upstream_unknown(),
                "WorkBuddy configuration has no client model list",
            )
        })?;
    for entry in agent_models {
        let name = entry.as_str().ok_or_else(|| {
            common::plugin_error(
                ErrorKind::upstream_unknown(),
                "invalid WorkBuddy client model list",
            )
        })?;
        if let Some(row) = rows
            .iter()
            .find(|row| row.get("id").and_then(Value::as_str) == Some(name))
            .or_else(|| {
                rows.iter()
                    .find(|row| row.get("name").and_then(Value::as_str) == Some(name))
            })
        {
            let id = row.get("id").and_then(Value::as_str).ok_or_else(|| {
                common::plugin_error(
                    ErrorKind::upstream_unknown(),
                    "model catalog entry has no ID",
                )
            })?;
            let free = pairs
                .iter()
                .find(|(_, line)| line["paidId"].as_str() == Some(id))
                .map(|(free, _)| free.as_str())
                .unwrap_or(id);
            if seen.insert(free.to_owned()) {
                let free_row = rows
                    .iter()
                    .find(|row| row.get("id").and_then(Value::as_str) == Some(free))
                    .unwrap_or(row);
                selected.push(free_row);
            }
        }
    }
    if selected.is_empty() {
        return Err(common::plugin_error(
            ErrorKind::upstream_unknown(),
            "WorkBuddy client model list has no catalog matches",
        ));
    }
    let mut response = parse_models(selected, "account-config")?;
    for model in &mut response.models {
        if let Some(line) = pairs.get(&model.id) {
            if let Some(name) = line
                .get("displayName")
                .and_then(Value::as_str)
                .filter(|name| !name.trim().is_empty())
            {
                name.clone_into(&mut model.display_name);
            }
            model
                .metadata
                .insert("workbuddy_paid_line".into(), line.clone());
        }
    }
    Ok(response)
}

/// 官方客户端的关思考判定：`onlyReasoning` 的模型恒推理；否则仅当
/// `reasoning.canDisableThinking` 未显式为 false 时可关。
fn can_disable_thinking(row: &Value) -> bool {
    row.get("onlyReasoning").and_then(Value::as_bool) != Some(true)
        && row
            .pointer("/reasoning/canDisableThinking")
            .and_then(Value::as_bool)
            != Some(false)
}

/// 目录的 supportedEfforts 不含关闭档。可关思考的模型补 "none"，宿主才会
/// 把 off 生成为 `reasoning_effort:"none"`（否则 off 档被隐藏）；
/// 不可关的模型保持原档位表，off 对其不可见。
fn effort_values(row: &Value, efforts: &[Value]) -> Vec<Value> {
    let mut values = efforts.to_vec();
    let has_none = values.iter().any(|value| {
        value
            .as_str()
            .is_some_and(|v| v.eq_ignore_ascii_case("none"))
    });
    if can_disable_thinking(row) && !has_none {
        values.insert(0, json!("none"));
    }
    values
}

fn parse_models<'a>(
    rows: impl IntoIterator<Item = &'a Value>,
    source: &str,
) -> Result<DiscoverResponse, PluginError> {
    let rows = rows.into_iter();
    let mut models = Vec::with_capacity(rows.size_hint().0);
    let mut seen = BTreeSet::new();
    for row in rows {
        let id = row
            .as_str()
            .or_else(|| row.get("id").and_then(Value::as_str))
            .ok_or_else(|| {
                common::plugin_error(
                    ErrorKind::upstream_unknown(),
                    "model catalog entry has no ID",
                )
            })?;
        if !valid_id(id) || !seen.insert(id) {
            return Err(common::plugin_error(
                ErrorKind::upstream_unknown(),
                "model catalog contains invalid or duplicate IDs",
            ));
        }
        let mut metadata = BTreeMap::from([("workbuddy_catalog_source".into(), json!(source))]);
        for (from, to) in [
            ("supportsToolCall", "tool_call"),
            ("supportsReasoning", "reasoning"),
        ] {
            if let Some(value) = row.get(from).and_then(Value::as_bool) {
                metadata.insert(to.into(), json!(value));
            }
        }
        let mut limit = serde_json::Map::new();
        for (from, to) in [
            ("maxAllowedSize", "context"),
            ("maxInputTokens", "input"),
            ("maxOutputTokens", "output"),
        ] {
            if let Some(value) = row.get(from).and_then(Value::as_u64) {
                limit.insert(to.into(), json!(value));
            }
        }
        if !limit.is_empty() {
            metadata.insert("limit".into(), Value::Object(limit));
        }
        if let Some(images) = row.get("supportsImages").and_then(Value::as_bool) {
            let input = if images {
                vec!["text", "image"]
            } else {
                vec!["text"]
            };
            metadata.insert(
                "modalities".into(),
                json!({"input": input, "output": ["text"]}),
            );
        }
        if let Some(efforts) = row
            .pointer("/reasoning/supportedEfforts")
            .and_then(Value::as_array)
        {
            metadata.insert(
                "reasoning_options".into(),
                json!([{"type":"effort", "values":effort_values(row, efforts)}]),
            );
        }
        // 目录的 reasoning.effort 是固定档、defaultEffort 是默认档；
        // 推理缺省注入 effort 时优先取目录声明值。
        if let Some(effort) = row
            .pointer("/reasoning/defaultEffort")
            .or_else(|| row.pointer("/reasoning/effort"))
            .and_then(Value::as_str)
        {
            metadata.insert("reasoning_default_effort".into(), json!(effort));
        }
        models.push(DiscoveredModel {
            id: id.into(),
            display_name: row.get("name").and_then(Value::as_str).unwrap_or(id).into(),
            family: None,
            selector: None,
            capabilities: Vec::new(),
            metadata,
        });
    }
    Ok(DiscoverResponse {
        models,
        next_cursor: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_ambiguous_model_configuration() {
        for text in ["glm-5.3\nglm-5.3", "glm 5.3", "model\u{0000}"] {
            let options = BTreeMap::from([("model_ids".into(), json!(text))]);
            assert_eq!(validate(&options).issues[0].code, "invalid_model_ids");
        }
        let options = BTreeMap::from([("model_ids".into(), json!("glm-5.3\n\nkimi-k3-1"))]);
        assert_eq!(
            configured_ids(&options).unwrap(),
            vec!["glm-5.3", "kimi-k3-1"]
        );
    }

    #[test]
    fn off_level_is_advertised_only_for_models_that_can_disable_thinking() {
        let rows: Vec<Value> = serde_json::from_str(
            r#"[
            {"id":"deepseek-v4-pro","onlyReasoning":false,"supportsReasoning":true,
             "reasoning":{"canDisableThinking":true,"supportedEfforts":["high","xhigh"]}},
            {"id":"glm-5.3","onlyReasoning":true,"supportsReasoning":true,
             "reasoning":{"canDisableThinking":true,"supportedEfforts":["low","high","max"]}},
            {"id":"hy3","onlyReasoning":true,"supportsReasoning":true,
             "reasoning":{"canDisableThinking":false,"supportedEfforts":["low","high"]}},
            {"id":"x-nodisable","onlyReasoning":false,"supportsReasoning":true,
             "reasoning":{"canDisableThinking":false,"supportedEfforts":["high"]}}
        ]"#,
        )
        .unwrap();
        let models = parse_models(&rows, "test").unwrap().models;
        let values = |id: &str| {
            let model = models.iter().find(|model| model.id == id).unwrap();
            model.metadata["reasoning_options"][0]["values"].clone()
        };
        assert_eq!(values("deepseek-v4-pro"), json!(["none", "high", "xhigh"]));
        assert_eq!(values("glm-5.3"), json!(["low", "high", "max"]));
        assert_eq!(values("hy3"), json!(["low", "high"]));
        assert_eq!(values("x-nodisable"), json!(["high"]));
    }
}
