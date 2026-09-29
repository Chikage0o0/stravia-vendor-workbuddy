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
    if options.keys().any(|key| key != "model_ids") {
        issues.push(ValidationIssue {
            field: None,
            code: "unknown_option".into(),
            message: messages::invalid_options(),
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
    let payload: Value = serde_json::from_slice(&body).map_err(|_| {
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
    let config = payload.get("data").unwrap_or(&payload);
    let rows = config
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            common::plugin_error(
                ErrorKind::upstream_unknown(),
                "WorkBuddy configuration has no models array",
            )
        })?;
    parse_models(&curated_catalog(rows), "account-config")
}

/// 服务端下发的是全量后端目录，包含内部功能模型与同名部署变体。
/// 只保留带计费标签或标记为默认的条目；同名变体按 iconUrl 优先去重。
/// 过滤结果为空（目录结构变化或特殊账号形态）时回退全量列表。
fn curated_catalog(rows: &[Value]) -> Vec<Value> {
    let visible = |row: &Value| {
        row.get("credits").is_some_and(|v| match v {
            Value::String(s) => !s.trim().is_empty(),
            Value::Null => false,
            _ => true,
        }) || row.get("isDefault") == Some(&Value::Bool(true))
    };
    let mut curated: Vec<&Value> = Vec::new();
    let mut slots: BTreeMap<&str, usize> = BTreeMap::new();
    for row in rows.iter().filter(|row| visible(row)) {
        let name = row.get("name").and_then(Value::as_str).unwrap_or_default();
        match slots.get(name).copied() {
            None => {
                slots.insert(name, curated.len());
                curated.push(row);
            }
            Some(i)
                if curated[i].get("iconUrl").is_none() && row.get("iconUrl").is_some() =>
            {
                curated[i] = row;
            }
            _ => {}
        }
    }
    if curated.is_empty() {
        return rows.to_vec();
    }
    curated.into_iter().cloned().collect()
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
    let has_none = values
        .iter()
        .any(|value| value.as_str().is_some_and(|v| v.eq_ignore_ascii_case("none")));
    if can_disable_thinking(row) && !has_none {
        values.insert(0, json!("none"));
    }
    values
}

fn parse_models(rows: &[Value], source: &str) -> Result<DiscoverResponse, PluginError> {
    let mut models = Vec::with_capacity(rows.len());
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
    fn curated_catalog_drops_internal_and_dedupes_deployment_variants() {
        let rows: Vec<Value> = serde_json::from_str(
            r#"[
            {"id":"auto","name":"Auto","isDefault":true},
            {"id":"hy3","name":"Hy3","credits":"x0.00"},
            {"id":"hy3-b","name":"Hy3","credits":"x0.00"},
            {"id":"hy3-x","name":"Hy3","credits":"x0.05","iconUrl":"data:,"},
            {"id":"glm-5.3","name":"GLM-5.3","credits":"x0.79"},
            {"id":"codewise-jump","name":"codewise-jump"},
            {"id":"seedance-2.5","name":"Seedance-2.5","tags":["text-to-video"]}
        ]"#,
        )
        .unwrap();
        let curated = curated_catalog(&rows);
        let ids: Vec<&str> = curated
            .iter()
            .map(|row| row["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["auto", "hy3-x", "glm-5.3"]);
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

    #[test]
    fn curated_catalog_falls_back_when_everything_is_filtered() {
        let rows: Vec<Value> = serde_json::from_str(
            r#"[{"id":"a","name":"A"},{"id":"b","name":"B"}]"#,
        )
        .unwrap();
        assert_eq!(curated_catalog(&rows).len(), 2);
    }
}
