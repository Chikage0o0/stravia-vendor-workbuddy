//! WorkBuddy `/v2/chat/completions` 推理通道。
//!
//! 请求由共享编解码器产出 canonical 请求体，完成上游线规修正后按官方 CLI
//! 的字段白名单过滤；请求头只使用插件生成且在白名单中的字段，未知参数静默丢弃。
//! 上游响应固定是 SSE 流，先经有界增量规范化（重复 `data:` 前缀、
//! 心跳注释、空 `function_call` 占位、`{"code":N}` 业务错误包），
//! 再交给共享流解码器逐事件解出 canonical 增量。

use std::borrow::Cow;

use serde_json::{Value, json};
use stravia_protocol_codec::accumulator::StreamResponseAccumulator;
use stravia_protocol_codec::transform::{ProtocolTransform, StreamDecodeStage};
use stravia_runtime_contract::protocol::ids::ProtocolEndpoint;
use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_vendor_common::common;
use stravia_vendor_sdk::{
    AiErrorKind, AiStreamDelta, ErrorKind, GuestHost, HttpRequest, HttpResponse, ModelMetadata,
    OperationOutput, PluginError, ProviderSnapshot, read_http_body,
};

use crate::{PROTOCOL, Region, auth};

/// 网关固定的对话补全路径。
const CHAT_PATH: &str = "/v2/chat/completions";
/// 上游要求首条必须是 system 消息时的兜底内容。
const DEFAULT_SYSTEM_PROMPT: &str = "You are a helpful assistant.";
const MAX_ERROR_BODY: usize = 256 * 1024;
const MAX_UNARY_BODY: usize = 32 * 1024 * 1024;
/// 单个 SSE 事件未遇行尾时缓冲上限；防止畸形流让行缓冲无限增长。
const MAX_SSE_EVENT_BYTES: usize = 8 * 1024 * 1024;

pub(crate) fn execute(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    region: Region,
    mut request: AiRequest,
) -> Result<OperationOutput, PluginError> {
    let model = provider
        .model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .ok_or_else(|| {
            common::plugin_error(ErrorKind::Invalid, "WorkBuddy inference requires a model")
        })?;
    if !crate::models::validate(&provider.options).issues.is_empty() {
        return Err(common::plugin_error(
            ErrorKind::Invalid,
            "invalid WorkBuddy model options",
        ));
    }
    let enabled = provider
        .options
        .get("auto_paid_on_rate_limit")
        .and_then(Value::as_bool)
        == Some(true);
    request.stream.enabled = true;
    let mut first_auth_headers = Some(auth::inference_headers(provider, region)?);
    let mut pair = if enabled {
        provider
            .model_metadata
            .as_ref()
            .and_then(|meta| meta.extensions.get("workbuddy_paid_line"))
            .cloned()
    } else {
        None
    };
    let root = if enabled {
        Some(crate::state::read(host)?)
    } else {
        None
    };
    if let Some(windows) = root
        .as_ref()
        .and_then(|root| root.get("workbuddy_paid_windows"))
        && !windows
            .as_object()
            .is_some_and(|windows| windows.values().all(|reset| reset.as_i64().is_some()))
    {
        return Err(common::plugin_error(
            ErrorKind::Invalid,
            "stored WorkBuddy paid windows are malformed",
        ));
    }
    let mut pair_lookup_done = false;
    if pair.is_none()
        && root
            .as_ref()
            .is_some_and(|root| has_active_window(root, provider, region, model))
    {
        pair = crate::models::paid_line(host, provider, region, model)?;
        pair_lookup_done = true;
    }
    let mut paid = false;
    if let (Some(pair), Some(root)) = (pair.as_ref().filter(|pair| valid_pair(pair, model)), &root)
    {
        let key = window_key(provider, region, pair);
        paid = root
            .get("workbuddy_paid_windows")
            .and_then(|windows| windows.get(&key))
            .and_then(Value::as_i64)
            .is_some_and(|reset| reset > chrono::Utc::now().timestamp_millis());
    }
    // 免费频控与付费线路属于同一次逻辑补全；宿主只接收一次 UpstreamStarted。
    // 第二次传输必须在输出提交前发生，不能递归执行完整推理入口。
    for attempt in 0..2 {
        let paid_metadata = if paid {
            let Some(Value::Object(mut fields)) = pair.take() else {
                return Err(common::plugin_error(
                    ErrorKind::Invalid,
                    "invalid WorkBuddy paid model metadata",
                ));
            };
            let Some(Value::String(paid_id)) = fields.remove("paidId") else {
                return Err(common::plugin_error(
                    ErrorKind::Invalid,
                    "invalid WorkBuddy paid model metadata",
                ));
            };
            request.model = paid_id;
            let metadata = fields.remove("paidMetadata").ok_or_else(|| {
                common::plugin_error(ErrorKind::Invalid, "invalid WorkBuddy paid model metadata")
            })?;
            // 规范化只消费扩展字段；实际线路 ID 已在 request.model 中，无需复制。
            Some(ModelMetadata {
                extensions: serde_json::from_value(metadata).map_err(|_| {
                    common::plugin_error(
                        ErrorKind::Invalid,
                        "invalid WorkBuddy paid model metadata",
                    )
                })?,
                ..Default::default()
            })
        } else {
            request.model = model.to_owned();
            None
        };
        let encoded = common::encode_inference_request(PROTOCOL, &request)?;
        let mut body = encoded.body;
        normalize_request_body(
            &mut body,
            if paid {
                paid_metadata.as_ref()
            } else {
                provider.model_metadata.as_ref()
            },
        );
        retain_cli_body(&mut body);
        let body = serde_json::to_vec(&body).map_err(|error| {
            common::plugin_error(
                ErrorKind::Invalid,
                format!("failed to serialize request body: {error}"),
            )
        })?;
        // 上游身份与会话头只由插件构造，不接收 codec 或客户端自带的标头。
        let mut headers = match first_auth_headers.take() {
            Some(headers) => headers,
            None => auth::inference_headers(provider, region)?,
        };
        set_header(&mut headers, "accept", "application/json");
        apply_session_headers(&mut headers, provider);
        retain_cli_headers(&mut headers);
        if attempt == 0 {
            host.emit_started()?;
        }
        let response = host.http_start(HttpRequest {
            method: "POST".into(),
            url: format!("{}{CHAT_PATH}", region.origin()),
            headers,
            body,
        })?;
        let mut rate_limit = None;
        let mut deferred_deltas = Vec::new();
        let result = decode_attempt(
            host,
            response,
            enabled
                && !paid
                && attempt == 0
                && pair.as_ref().is_none_or(|pair| valid_pair(pair, model)),
            &mut rate_limit,
            &mut deferred_deltas,
        );
        let Some(rate_limit) = rate_limit else {
            return result;
        };
        let reset = reset_at(&rate_limit);
        // 已到期的频控错误不再授权付费；缺少时间时只允许当前请求切换。
        if pair.is_none()
            && !pair_lookup_done
            && reset.is_none_or(|reset| reset > chrono::Utc::now().timestamp_millis())
        {
            pair = crate::models::paid_line(host, provider, region, model)?;
        }
        let Some(pair_value) = pair
            .as_ref()
            .filter(|pair| valid_pair(pair, model))
            .filter(|_| reset.is_none_or(|reset| reset > chrono::Utc::now().timestamp_millis()))
        else {
            if !deferred_deltas.is_empty() {
                common::emit_deltas(
                    host,
                    &mut StreamResponseAccumulator::default(),
                    &deferred_deltas,
                )?;
            }
            return result;
        };
        if let Some(reset) = reset {
            // SDK 没有 CAS；请求结束后重新读取，避免用旧快照覆盖认证待登录字段。
            let mut root = crate::state::read(host)?;
            let windows = root
                .entry("workbuddy_paid_windows")
                .or_insert_with(|| json!({}));
            let windows = windows
                .as_object_mut()
                .filter(|windows| windows.values().all(|reset| reset.as_i64().is_some()))
                .ok_or_else(|| {
                    common::plugin_error(
                        ErrorKind::Invalid,
                        "stored WorkBuddy paid windows are malformed",
                    )
                })?;
            windows.insert(window_key(provider, region, pair_value), json!(reset));
            crate::state::write(host, root)?;
        }
        paid = true;
    }
    unreachable!("at most two line attempts")
}

fn valid_pair(pair: &Value, model: &str) -> bool {
    let valid_id = |id: &str| {
        !id.is_empty()
            && id.len() <= 200
            && !id.chars().any(char::is_whitespace)
            && !id.chars().any(char::is_control)
    };
    pair.get("freeId").and_then(Value::as_str) == Some(model)
        && pair.get("allowPaidSwitch").and_then(Value::as_bool) == Some(true)
        && valid_id(model)
        && pair
            .get("paidId")
            .and_then(Value::as_str)
            .is_some_and(|paid| paid != model && valid_id(paid))
        && pair.get("paidMetadata").is_some_and(Value::is_object)
}

fn window_key(provider: &ProviderSnapshot, region: Region, pair: &Value) -> String {
    // 身份字段可能含分隔符，JSON 元组编码避免不同账号或线路发生键碰撞。
    serde_json::to_string(&(
        provider.provider_id.as_str(),
        region.id(),
        provider.credentials.get("uid"),
        provider.credentials.get("enterprise_id"),
        pair.get("freeId"),
        pair.get("paidId"),
    ))
    .expect("JSON identity tuple is serializable")
}

fn has_active_window(
    root: &serde_json::Map<String, Value>,
    provider: &ProviderSnapshot,
    region: Region,
    free_id: &str,
) -> bool {
    let now = chrono::Utc::now().timestamp_millis();
    root.get("workbuddy_paid_windows")
        .and_then(Value::as_object)
        .is_some_and(|windows| {
            windows.iter().any(|(key, reset)| {
                if !reset.as_i64().is_some_and(|reset| reset > now) {
                    return false;
                }
                let Ok(identity) = serde_json::from_str::<[Value; 6]>(key) else {
                    return false;
                };
                identity[0].as_str() == Some(provider.provider_id.as_str())
                    && identity[1].as_str() == Some(region.id())
                    && identity[2] == *provider.credentials.get("uid").unwrap_or(&Value::Null)
                    && identity[3]
                        == *provider
                            .credentials
                            .get("enterprise_id")
                            .unwrap_or(&Value::Null)
                    && identity[4].as_str() == Some(free_id)
            })
        })
}

fn reset_at(value: &Value) -> Option<i64> {
    let reset = value
        .get("resetAt")
        .or_else(|| value.pointer("/data/resetAt"))
        .or_else(|| value.pointer("/error/resetAt"));
    if let Some(reset) = reset {
        if let Some(number) = reset
            .as_i64()
            .or_else(|| reset.as_str().and_then(|text| text.parse().ok()))
        {
            return if number > 100_000_000_000 {
                Some(number)
            } else {
                number.checked_mul(1000)
            };
        }
        if let Some(time) = reset
            .as_str()
            .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
        {
            return Some(time.timestamp_millis());
        }
    }
    let message = value
        .get("message")
        .or_else(|| value.pointer("/error/message"))
        .and_then(Value::as_str)?;
    // 客户端将错误文案中的无时区日期解释为上海时间；只使用实际日期，不假定窗口时长。
    for (start, _) in message.char_indices() {
        let Some(text) = message.get(start..start + 19) else {
            continue;
        };
        if let Ok(time) = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S")
            .or_else(|_| chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S"))
        {
            return Some(time.and_utc().timestamp_millis() - 8 * 60 * 60 * 1000);
        }
    }
    None
}

fn explicit_rate_limit(value: &Value) -> bool {
    frame_error_code(value) == Some(6004)
        || value
            .pointer("/error/code")
            .is_some_and(|code| code.as_i64() == Some(6004) || code.as_str() == Some("6004"))
}

fn set_header(headers: &mut Vec<(String, String)>, name: &str, value: &str) {
    headers.retain(|(key, _)| !key.eq_ignore_ascii_case(name));
    headers.push((name.to_owned(), value.to_owned()));
}

/// 来自官方 CLI 的默认传输头、身份拦截器、会话与 OTel 注入器。
/// 不允许自定义透传配置扩大集合；认证值仍由插件自己的区域凭据产生。
fn retain_cli_headers(headers: &mut Vec<(String, String)>) {
    const ALLOWED: &[&str] = &[
        "content-type",
        "accept",
        "authorization",
        "user-agent",
        "x-requested-with",
        "x-agent-purpose",
        "x-ide-name",
        "x-ide-type",
        "x-ide-version",
        "x-product",
        "x-domain",
        "x-user-id",
        "x-enterprise-id",
        "x-tenant-id",
        "x-request-id",
        "x-conversation-id",
        "x-conversation-message-id",
        "x-conversation-request-id",
        "baggage",
        "traceparent",
        "x-trace-id",
        "x-b3-traceid",
        "x-b3-spanid",
        "x-b3-sampled",
    ];
    headers.retain(|(key, _)| {
        ALLOWED
            .iter()
            .any(|allowed| key.eq_ignore_ascii_case(allowed))
    });
}

/// 官方客户端为推理请求注入的会话链与 OTel 传播标头。
///
/// `X-Conversation-ID` 是上游网关做前缀缓存亲和的稳定路由键，取宿主按
/// 本地链路派生的 `operation_metadata.session_affinity`（64 位 hex，同一
/// 链路各轮不变），不采用客户端自报的 session。缺失时不伪造随机值——
/// 每请求一枚随机会话键会进一步打散上游的亲和视图，比缺失更差。
///
/// 官方线规中 `X-Request-ID` 与 `X-Conversation-Message-ID` 同为每轮
/// 生成的 messageId，这里共用一枚；轮次级 ID 由插件生成，语义等价。
fn apply_session_headers(headers: &mut Vec<(String, String)>, provider: &ProviderSnapshot) {
    let message_id = uuid::Uuid::new_v4().simple().to_string();
    set_header(headers, "x-request-id", &message_id);
    set_header(headers, "x-conversation-message-id", &message_id);
    set_header(
        headers,
        "x-conversation-request-id",
        &uuid::Uuid::new_v4().simple().to_string(),
    );
    if let Some(conversation_id) = provider
        .operation_metadata
        .get("session_affinity")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        set_header(headers, "x-conversation-id", conversation_id);
        set_header(
            headers,
            "baggage",
            &format!(
                "codebuddy.session_id={conversation_id},codebuddy.conversation_request_id={message_id}"
            ),
        );
    }
    // 本地生成根 span 的 W3C/B3 追踪标头；tracestate 没有可传播的入站值，不发。
    let trace_id = uuid::Uuid::new_v4().simple().to_string();
    let span_id = uuid::Uuid::new_v4().simple().to_string()[..16].to_owned();
    set_header(
        headers,
        "traceparent",
        &format!("00-{trace_id}-{span_id}-01"),
    );
    set_header(headers, "x-trace-id", &trace_id);
    set_header(headers, "x-b3-traceid", &trace_id);
    set_header(headers, "x-b3-spanid", &span_id);
    set_header(headers, "x-b3-sampled", "1");
}

/// 编码后请求体的线规修正，逐项对应上游校验错误码：
/// developer→system（11128）、首条 system 兜底、tool_choice 归一（11101）、
/// max_completion_tokens 别名、deepseek 思考模式 reasoning 回填（11155）。
/// 不做指纹清洗，不改写用户提示词。
fn normalize_request_body(body: &mut Value, model_metadata: Option<&ModelMetadata>) {
    normalize_tool_choice(body);
    translate_max_completion_tokens(body);
    let is_deepseek = body
        .get("model")
        .and_then(Value::as_str)
        .is_some_and(|model| model.to_ascii_lowercase().starts_with("deepseek"));
    // 思考字段先落成最终线上形态，历史回填门槛以序列化结果为准。
    let thinking_enabled = if is_deepseek {
        serialize_deepseek_thinking(body, model_metadata)
    } else {
        drop_none_effort(body);
        false
    };
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for message in messages.iter_mut() {
        if let Some(object) = message.as_object_mut()
            && object.get("role").and_then(Value::as_str) == Some("developer")
        {
            object.insert("role".into(), Value::String("system".into()));
        }
    }
    if messages
        .first()
        .and_then(|message| message.get("role"))
        .and_then(Value::as_str)
        != Some("system")
    {
        messages.insert(
            0,
            json!({"role": "system", "content": DEFAULT_SYSTEM_PROMPT}),
        );
    }
    if is_deepseek {
        backfill_reasoning(messages, thinking_enabled);
    }
}

/// 白名单取自官方 `@tencent-ai/codebuddy-code@2.161.2` 的
/// OpenAIChatCompletionsModel、消息转换器与产品思考参数构造。
/// 只筛协议结构；JSON Schema、工具 arguments 和文本是用户数据，不递归清洗。
fn retain_cli_body(body: &mut Value) {
    retain_fields(
        body,
        &[
            "model",
            "messages",
            "tools",
            "temperature",
            "top_p",
            "frequency_penalty",
            "presence_penalty",
            "max_tokens",
            "tool_choice",
            "parallel_tool_calls",
            "stream",
            "stream_options",
            "store",
            "prompt_cache_retention",
            "response_format",
            "reasoning_effort",
            "verbosity",
            "thinking",
            "reasoning",
            "reasoning_summary",
            "enable_thinking",
            "chat_template_kwargs",
        ],
    );
    if let Some(options) = body.get_mut("stream_options") {
        retain_fields(options, &["include_usage"]);
    }
    if let Some(thinking) = body.get_mut("thinking") {
        retain_fields(thinking, &["type", "clear_thinking"]);
    }
    if let Some(reasoning) = body.get_mut("reasoning") {
        retain_fields(reasoning, &["effort", "enabled"]);
    }
    if let Some(template) = body.get_mut("chat_template_kwargs") {
        retain_fields(template, &["enable_thinking", "preserve_thinking"]);
    }
    if let Some(format) = body.get_mut("response_format") {
        retain_fields(format, &["type", "json_schema"]);
        if let Some(schema) = format.get_mut("json_schema") {
            retain_fields(schema, &["name", "strict", "schema"]);
        }
    }
    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            retain_fields(tool, &["type", "function"]);
            if let Some(function) = tool.get_mut("function") {
                retain_fields(function, &["name", "description", "parameters", "strict"]);
            }
        }
    }
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages {
            retain_fields(
                message,
                &[
                    "role",
                    "content",
                    "name",
                    "tool_calls",
                    "tool_call_id",
                    "reasoning",
                    "reasoning_content",
                    "extra_fields",
                    "audio",
                ],
            );
            if let Some(extra) = message.get_mut("extra_fields") {
                retain_fields(extra, &["_meta", "google"]);
                retain_google_signature(extra);
            }
            if let Some(audio) = message.get_mut("audio") {
                retain_fields(audio, &["id"]);
            }
            if let Some(calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut) {
                for call in calls {
                    retain_fields(call, &["id", "type", "function", "extra_content"]);
                    if let Some(extra) = call.get_mut("extra_content") {
                        retain_fields(extra, &["google"]);
                        retain_google_signature(extra);
                    }
                    if let Some(function) = call.get_mut("function") {
                        retain_fields(function, &["name", "arguments"]);
                    }
                }
            }
            if let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) {
                for part in content {
                    retain_fields(
                        part,
                        &[
                            "type",
                            "text",
                            "refusal",
                            "image_url",
                            "input_audio",
                            "file",
                            "cache_control",
                        ],
                    );
                    if let Some(cache) = part.get_mut("cache_control") {
                        retain_fields(cache, &["type"]);
                    }
                    if let Some(image) = part.get_mut("image_url") {
                        retain_fields(image, &["url", "detail"]);
                    }
                    if let Some(audio) = part.get_mut("input_audio") {
                        retain_fields(audio, &["data", "format"]);
                    }
                    if let Some(file) = part.get_mut("file") {
                        retain_fields(file, &["file_data", "file_id", "filename"]);
                    }
                }
            }
        }
    }
}

fn retain_google_signature(value: &mut Value) {
    if let Some(google) = value.get_mut("google") {
        retain_fields(google, &["thought_signature"]);
    }
}

fn retain_fields(value: &mut Value, allowed: &[&str]) {
    if let Some(object) = value.as_object_mut() {
        object.retain(|key, _| allowed.contains(&key.as_str()));
    }
}

/// 上游不接受 "none" 语义和未知对象形态的 tool_choice。
fn normalize_tool_choice(body: &mut Value) {
    let Some(tool_choice) = body.get("tool_choice").cloned() else {
        return;
    };
    match tool_choice {
        Value::String(choice) => {
            if choice.trim().eq_ignore_ascii_case("none") {
                drop_tool_choice(body);
            }
        }
        Value::Object(_) => {
            let kind = tool_choice
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            match kind.as_str() {
                "none" => drop_tool_choice(body),
                "auto" | "required" => {
                    if let Some(object) = body.as_object_mut() {
                        object.insert("tool_choice".into(), Value::String(kind));
                    }
                }
                // 上游对具名调用的写法是裸函数名字符串。
                "function" => {
                    let name = tool_choice
                        .pointer("/function/name")
                        .and_then(Value::as_str)
                        .or_else(|| tool_choice.get("name").and_then(Value::as_str))
                        .unwrap_or("")
                        .trim()
                        .to_owned();
                    if let Some(object) = body.as_object_mut() {
                        object.insert(
                            "tool_choice".into(),
                            Value::String(if name.is_empty() { "auto".into() } else { name }),
                        );
                    }
                }
                _ => {
                    if let Some(object) = body.as_object_mut() {
                        object.remove("tool_choice");
                    }
                }
            }
        }
        _ => {
            if let Some(object) = body.as_object_mut() {
                object.remove("tool_choice");
            }
        }
    }
}

fn drop_tool_choice(body: &mut Value) {
    if let Some(object) = body.as_object_mut() {
        object.remove("tool_choice");
        object.remove("tools");
        object.remove("functions");
    }
}

/// 上游只认 `max_tokens`。
fn translate_max_completion_tokens(body: &mut Value) {
    let Some(object) = body.as_object_mut() else {
        return;
    };
    let Some(alias) = object.remove("max_completion_tokens") else {
        return;
    };
    if object.contains_key("max_tokens") {
        return;
    }
    let parsed = alias
        .as_i64()
        .or_else(|| alias.as_u64().and_then(|v| i64::try_from(v).ok()))
        .or_else(|| alias.as_f64().map(|v| v as i64))
        .or_else(|| alias.as_str().and_then(|s| s.trim().parse().ok()));
    if let Some(value) = parsed.filter(|value| *value > 0) {
        object.insert("max_tokens".into(), Value::from(value));
    }
}

/// 非 deepseek 模型的关思考：上游档位表没有 "none"，官方客户端关思考时
/// 只是不发 reasoning 字段；宿主 off 档产出的 `reasoning_effort:"none"`
/// 在此还原成同样的线规形态。
fn drop_none_effort(body: &mut Value) {
    let Some(object) = body.as_object_mut() else {
        return;
    };
    if object
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .is_some_and(|effort| effort.trim().eq_ignore_ascii_case("none"))
    {
        object.remove("reasoning_effort");
    }
}

/// 官方客户端 thinkingFormat=deepseek 的线规形态：
///   开启 → `thinking:{type:"enabled"}`（客户端自带 type 不覆盖）+
///          `reasoning_effort` 档位（缺省补目录默认档，再兜底 "high"）+
///          `reasoning_summary:"auto"`；
///   关闭 → `thinking:{type:"disabled"}` 并删除 `reasoning_effort`——
///          上游档位表没有 "none"，关的语义是删字段。
/// `thinking.type` 单独不足以驱动推理：上游实测只在 effort 到位时产出
/// reasoning_content。返回最终思考状态，供历史回填门槛判断。
fn serialize_deepseek_thinking(body: &mut Value, model_metadata: Option<&ModelMetadata>) -> bool {
    let Some(object) = body.as_object_mut() else {
        return false;
    };
    // reasoningEffort 旧写法归并到 reasoning_effort。
    if let Some(camel) = object.remove("reasoningEffort") {
        object.entry("reasoning_effort".to_owned()).or_insert(camel);
    }
    let thinking_type = object
        .get("thinking")
        .and_then(|thinking| thinking.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let effort = object
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if thinking_type == "disabled" || effort.eq_ignore_ascii_case("none") {
        object.insert("thinking".into(), json!({"type": "disabled"}));
        object.remove("reasoning_effort");
        return false;
    }
    // 目录明确标注不支持推理的模型不注入默认档；元数据缺失按可推理处理
    // （官方客户端对 supportsReasoning 缺省同样走兜底档位）。此时仅在
    // 客户端显式表达思考意图时按开启处理。
    if model_metadata
        .and_then(|meta| meta.extensions.get("reasoning"))
        .and_then(Value::as_bool)
        == Some(false)
    {
        return thinking_type == "enabled" || !effort.is_empty();
    }
    let thinking = object
        .entry("thinking".to_owned())
        .or_insert_with(|| json!({}));
    if !thinking.is_object() {
        *thinking = json!({});
    }
    thinking
        .as_object_mut()
        .expect("thinking is an object")
        .entry("type".to_owned())
        .or_insert_with(|| json!("enabled"));
    if object.get("reasoning_effort").is_none() {
        let default_effort = model_metadata
            .and_then(|meta| meta.extensions.get("reasoning_default_effort"))
            .and_then(Value::as_str)
            .unwrap_or("high");
        object.insert("reasoning_effort".into(), json!(default_effort));
    }
    object
        .entry("reasoning_summary".to_owned())
        .or_insert_with(|| json!("auto"));
    true
}

/// 上游错误码 11155：思考开启或
/// 历史中已有推理痕迹时，每条 assistant 消息必须带字符串
/// `reasoning_content` 与非空 `reasoning`。真实推理文本已由共享编解码器
/// 保留在 `reasoning_content`；这里只补缺失字段，缺失内容用空格占位
/// （只满足上游长度校验，不伪造推理文本）。
fn backfill_reasoning(messages: &mut [Value], thinking_enabled: bool) {
    let has_trace = messages.iter().any(|message| {
        message
            .get("reasoning")
            .and_then(Value::as_str)
            .is_some_and(|reasoning| !reasoning.is_empty())
            || message.get("reasoning_content").is_some()
    });
    if !thinking_enabled && !has_trace {
        return;
    }
    for message in messages.iter_mut() {
        let Some(object) = message.as_object_mut() else {
            continue;
        };
        if object.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let reasoning_content = object
            .get("reasoning_content")
            .and_then(Value::as_str)
            .or_else(|| object.get("reasoning").and_then(Value::as_str))
            .unwrap_or("")
            .to_owned();
        if !object
            .get("reasoning_content")
            .is_some_and(Value::is_string)
        {
            object.insert(
                "reasoning_content".into(),
                Value::String(reasoning_content.clone()),
            );
        }
        let reasoning_present = object
            .get("reasoning")
            .and_then(Value::as_str)
            .is_some_and(|reasoning| !reasoning.is_empty());
        if !reasoning_present {
            object.insert(
                "reasoning".into(),
                Value::String(if reasoning_content.is_empty() {
                    " ".into()
                } else {
                    reasoning_content
                }),
            );
        }
    }
}

fn decode_attempt(
    host: &GuestHost,
    response: HttpResponse,
    eligible: bool,
    rate_limit: &mut Option<Value>,
    deferred_deltas: &mut Vec<AiStreamDelta>,
) -> Result<OperationOutput, PluginError> {
    let status = response.status()?;
    let response_headers = response.headers()?;
    if !(200..300).contains(&status) {
        let body = read_http_body(&response, MAX_ERROR_BODY)?;
        if eligible
            && let Ok(value) = serde_json::from_slice::<Value>(&body)
            && explicit_rate_limit(&value)
        {
            *rate_limit = Some(value);
        }
        let mut error = common::upstream_error(status, &response_headers, &body);
        error.message = format!("WorkBuddy inference HTTP {status}");
        return Err(error);
    }
    if is_event_stream(&response_headers) {
        decode_stream_attempt(host, response, eligible, rate_limit, deferred_deltas)
    } else {
        // 网关始终回 SSE；非流式响应仍按共享一元解码兜底。
        decode_unary_attempt(host, response, eligible, rate_limit)
    }
}

fn is_event_stream(headers: &[(String, String)]) -> bool {
    headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("content-type")
            && value.to_ascii_lowercase().contains("text/event-stream")
    })
}

/// HTTP 200 里的业务错误包（`{"error":…}` 或 `{"code":N,…}`）不能走成功路径。
fn in_band_error(value: &Value) -> Option<PluginError> {
    let error = value.get("error").filter(|error| !error.is_null());
    let code = frame_error_code(value);
    if error.is_none() && code.is_none() {
        return None;
    }
    let detail = match code {
        Some(code) => format!("WorkBuddy upstream error {code}"),
        None => "WorkBuddy upstream returned an error".into(),
    };
    Some(common::model_error(AiErrorKind::ServerError, detail))
}

fn decode_unary_attempt(
    host: &GuestHost,
    response: HttpResponse,
    eligible: bool,
    rate_limit: &mut Option<Value>,
) -> Result<OperationOutput, PluginError> {
    let body = read_http_body(&response, MAX_UNARY_BODY)?;
    let value: Value = serde_json::from_slice(&body).map_err(|error| {
        common::model_error(
            AiErrorKind::ServerError,
            format!("upstream returned invalid JSON: {error}"),
        )
    })?;
    if let Some(error) = in_band_error(&value) {
        if eligible && explicit_rate_limit(&value) {
            *rate_limit = Some(value);
        }
        return Err(error);
    }
    if !value
        .pointer("/choices/0/message")
        .is_some_and(Value::is_object)
        || !value
            .pointer("/choices/0/finish_reason")
            .and_then(Value::as_str)
            .is_some_and(|reason| !reason.is_empty())
    {
        return Err(common::model_error(
            AiErrorKind::UnexpectedEof,
            "WorkBuddy returned an incomplete completion",
        ));
    }
    let endpoint = common::endpoint(PROTOCOL)?;
    let complete = ProtocolTransform::global()
        .bind(endpoint, endpoint)
        .and_then(|pair| pair.decode_response(value))
        .map_err(common::map_response_transform_error)?;
    host.emit_completed(&complete)?;
    Ok(OperationOutput::Infer(Box::new(complete)))
}

fn decode_stream_attempt(
    host: &GuestHost,
    response: HttpResponse,
    eligible: bool,
    rate_limit: &mut Option<Value>,
    deferred_deltas: &mut Vec<AiStreamDelta>,
) -> Result<OperationOutput, PluginError> {
    let endpoint = common::endpoint(PROTOCOL)?;
    let mut interpreter = StreamInterpreter::new(endpoint)?;
    interpreter.normalizer.capture_rate_limit = eligible;
    let mut accumulator = StreamResponseAccumulator::default();
    let mut emitted = false;
    while let Some(chunk) = response.read_body()? {
        let deltas = interpreter.push(&chunk)?;
        if eligible
            && !emitted
            && !deltas.is_empty()
            && deltas
                .iter()
                .all(|delta| matches!(delta, AiStreamDelta::StreamError { .. }))
            && let Some(value) = interpreter.normalizer.rate_limit.take()
        {
            *rate_limit = Some(value);
            *deferred_deltas = deltas;
            return Err(common::model_error(
                AiErrorKind::ServerError,
                "WorkBuddy upstream error 6004",
            ));
        }
        emitted |= !deltas.is_empty();
        common::emit_deltas(host, &mut accumulator, &deltas)?;
    }
    let deltas = interpreter.finish()?;
    if eligible
        && !emitted
        && !deltas.is_empty()
        && deltas
            .iter()
            .all(|delta| matches!(delta, AiStreamDelta::StreamError { .. }))
        && let Some(value) = interpreter.normalizer.rate_limit.take()
    {
        *rate_limit = Some(value);
        *deferred_deltas = deltas;
        return Err(common::model_error(
            AiErrorKind::ServerError,
            "WorkBuddy upstream error 6004",
        ));
    }
    common::emit_deltas(host, &mut accumulator, &deltas)?;
    if !interpreter.terminated {
        // 截断或没有终止标记的流绝不能记成空成功：UnexpectedEof 经
        // emit_deltas 落成 Failed 终态并返回错误。
        let failure = common::emit_deltas(host, &mut accumulator, &[AiStreamDelta::UnexpectedEof]);
        return Err(failure.err().unwrap_or_else(|| {
            common::model_error(
                AiErrorKind::UnexpectedEof,
                "upstream stream ended without a terminal marker",
            )
        }));
    }
    let complete = accumulator.into_ai_response();
    host.emit_completed(&complete)?;
    Ok(OperationOutput::Infer(Box::new(complete)))
}

/// SSE 规范化器 + 共享流解码器：喂任意切块的上游字节，产出 canonical 增量。
/// `terminated` 同时要求实际 choice 的 finish_reason 与 `[DONE]`；
/// StreamError 由 emit_deltas 统一转失败，不计入终止。
struct StreamInterpreter {
    normalizer: SseNormalizer,
    decoder: StreamDecodeStage,
    terminated: bool,
}

impl StreamInterpreter {
    fn new(endpoint: ProtocolEndpoint) -> Result<Self, PluginError> {
        let decoder = ProtocolTransform::global()
            .decode_stream(endpoint)
            .map_err(common::map_response_transform_error)?;
        Ok(Self {
            normalizer: SseNormalizer::default(),
            decoder,
            terminated: false,
        })
    }

    fn push(&mut self, chunk: &[u8]) -> Result<Vec<AiStreamDelta>, PluginError> {
        let normalized = self.normalizer.feed(chunk)?;
        let mut deltas = self
            .decoder
            .decode_chunk(&normalized)
            .map_err(common::map_response_transform_error)?;
        redact_stream_errors(&mut deltas);
        self.terminated = self.normalizer.done && self.normalizer.finished_choice;
        Ok(deltas)
    }

    fn finish(&mut self) -> Result<Vec<AiStreamDelta>, PluginError> {
        let normalized = self.normalizer.finish()?;
        let mut deltas = self
            .decoder
            .decode_chunk(&normalized)
            .map_err(common::map_response_transform_error)?;
        deltas.extend(
            self.decoder
                .finish()
                .map_err(common::map_response_transform_error)?,
        );
        redact_stream_errors(&mut deltas);
        self.terminated = self.normalizer.done && self.normalizer.finished_choice;
        Ok(deltas)
    }
}

fn redact_stream_errors(deltas: &mut [AiStreamDelta]) {
    for delta in deltas {
        if let AiStreamDelta::StreamError { error } = delta {
            error.message = "WorkBuddy upstream stream failed".into();
            error.raw = None;
        }
    }
}

/// WorkBuddy 网关的 SSE 帧不规范：单行可能叠多个 `data:` 前缀、夹杂 `:`
/// 心跳注释、空 `data:` 行和 `function_call` 占位。这里只在事件边界上
/// 重写行，产出共享 OpenAI 流解码器直接消费的标准 `data: …\n\n` 序列。
/// 全程按字节处理——`\n` 不会出现在 UTF-8 多字节序列内，任意切块安全。
#[derive(Default)]
struct SseNormalizer {
    /// 尚未见到行尾的原始字节。
    pending: Vec<u8>,
    /// 当前事件是否已输出过 data 行（空事件不补边界）。
    event_open: bool,
    done: bool,
    finished_choice: bool,
    rate_limit: Option<Value>,
    capture_rate_limit: bool,
}

impl SseNormalizer {
    fn feed(&mut self, chunk: &[u8]) -> Result<Vec<u8>, PluginError> {
        let mut pending = std::mem::take(&mut self.pending);
        let previous_len = pending.len();
        pending.extend_from_slice(chunk);
        let mut out = Vec::new();
        let mut consumed = 0usize;
        for (index, byte) in pending.iter().enumerate().skip(previous_len) {
            if *byte != b'\n' {
                continue;
            }
            if index - consumed > MAX_SSE_EVENT_BYTES {
                return Err(common::plugin_error(
                    ErrorKind::ResourceExhausted,
                    "upstream SSE event exceeds the guest limit",
                ));
            }
            self.line(&pending[consumed..index], &mut out);
            consumed = index + 1;
        }
        pending.drain(..consumed);
        // 未结行的残余即当前半个事件，超过上限视为畸形流。
        if pending.len() > MAX_SSE_EVENT_BYTES {
            return Err(common::plugin_error(
                ErrorKind::ResourceExhausted,
                "upstream SSE event exceeds the guest limit",
            ));
        }
        self.pending = pending;
        Ok(out)
    }

    /// EOF 时把残余字节当作最后一行处理；畸形尾部交给解码器报错。
    fn finish(&mut self) -> Result<Vec<u8>, PluginError> {
        self.feed(b"\n\n")
    }

    fn line(&mut self, raw: &[u8], out: &mut Vec<u8>) {
        let line = if raw.last() == Some(&b'\r') {
            &raw[..raw.len() - 1]
        } else {
            raw
        };
        if line.iter().all(|byte| byte.is_ascii_whitespace()) {
            // 空行是事件边界；只给产出过 data 行的事件补分隔符。
            if self.event_open {
                out.push(b'\n');
                self.event_open = false;
            }
            return;
        }
        // `data:` 前缀可能被网关重复叠加，全部剥掉。
        let mut payload = trim_ascii_start(line);
        let mut prefixed = false;
        while let Some(rest) = payload.strip_prefix(b"data:") {
            prefixed = true;
            payload = trim_ascii_start(rest);
        }
        if !prefixed {
            // 裸 JSON 行 / 裸 [DONE] 一并接受；event:/id:/注释等字段行丢弃。
            if !(payload.starts_with(b"{") || payload == b"[DONE]".as_slice()) {
                return;
            }
        }
        // `data:` 空行和 `data: : ping` 心跳都是噪声。
        if payload.is_empty() || payload.starts_with(b":") {
            return;
        }
        let payload = self.normalize_payload(payload);
        out.extend_from_slice(b"data: ");
        out.extend_from_slice(&payload);
        out.push(b'\n');
        self.event_open = true;
    }

    /// 只有携带 `function_call` 或 `"code"` 的帧需要进 JSON 重写路径；
    /// 其余负载原样透传给共享解码器，parse 失败也原样交由解码器报错。
    fn normalize_payload<'a>(&mut self, payload: &'a [u8]) -> Cow<'a, [u8]> {
        if payload == b"[DONE]" {
            self.done = true;
            return Cow::Borrowed(payload);
        }
        if !contains_subslice(payload, b"function_call")
            && !contains_subslice(payload, b"\"code\"")
            && !contains_subslice(payload, b"\"finish_reason\"")
            && !contains_subslice(payload, b"\"error\"")
        {
            return Cow::Borrowed(payload);
        }
        let Ok(text) = std::str::from_utf8(payload) else {
            return Cow::Borrowed(payload);
        };
        let Ok(mut value) = serde_json::from_str::<Value>(text) else {
            return Cow::Borrowed(payload);
        };
        if self.capture_rate_limit && explicit_rate_limit(&value) && self.rate_limit.is_none() {
            self.rate_limit = Some(value.clone());
        }
        self.finished_choice |= value
            .pointer("/choices/0/finish_reason")
            .and_then(Value::as_str)
            .is_some_and(|reason| !reason.is_empty());
        if !rewrite_stream_frame(&mut value) {
            return Cow::Borrowed(payload);
        }
        match serde_json::to_vec(&value) {
            Ok(bytes) => Cow::Owned(bytes),
            Err(_) => Cow::Borrowed(payload),
        }
    }
}

fn trim_ascii_start(mut bytes: &[u8]) -> &[u8] {
    while let Some((first, rest)) = bytes.split_first() {
        if !matches!(*first, b' ' | b'\t') {
            break;
        }
        bytes = rest;
    }
    bytes
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

/// 帧级重写：`{"code":N,…}` 业务错误包转成编解码器认识的 `{"error":…}`；
/// `choices[].delta.function_call` 字典转成标准 `tool_calls` 续包。
fn rewrite_stream_frame(value: &mut Value) -> bool {
    if let Some(code) = frame_error_code(value) {
        *value = json!({"error": {"message": "WorkBuddy upstream stream error", "code": code}});
        return true;
    }
    let Some(choices) = value.get_mut("choices").and_then(Value::as_array_mut) else {
        return false;
    };
    let mut changed = false;
    for choice in choices {
        if choice.get("finish_reason").and_then(Value::as_str) == Some("function_call") {
            choice["finish_reason"] = json!("tool_calls");
            changed = true;
        }
        changed |= rewrite_function_call(choice);
    }
    changed
}

/// 顶层 `code` 非零（数字或数字字符串）即业务错误；带 `choices` 的常规块不是。
fn frame_error_code(value: &Value) -> Option<i64> {
    if value.get("choices").is_some() {
        return None;
    }
    value
        .get("code")
        .and_then(|code| code.as_i64().or_else(|| code.as_str()?.trim().parse().ok()))
        .filter(|code| *code != 0)
}

/// 上游的 `delta.function_call` 是字典而非 OpenAI 的 `tool_calls` 数组：
/// name 与 arguments 全空是流尾占位噪声；name 非空是真调用（映射 index 0）；
/// 只有 arguments 的是参数续包，绝不能丢。
fn rewrite_function_call(choice: &mut Value) -> bool {
    let Some(delta) = choice.get_mut("delta").and_then(Value::as_object_mut) else {
        return false;
    };
    let Some(function_call) = delta.remove("function_call") else {
        return false;
    };
    let Some(function_call) = function_call.as_object() else {
        return true;
    };
    let name = function_call
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let arguments = function_call
        .get("arguments")
        .and_then(Value::as_str)
        .unwrap_or("");
    if name.is_empty() && arguments.is_empty() {
        return true;
    }
    let mut function = serde_json::Map::new();
    if !name.is_empty() {
        function.insert("name".into(), Value::String(name.to_owned()));
    }
    if !arguments.is_empty() {
        function.insert("arguments".into(), Value::String(arguments.to_owned()));
    }
    let mut call = serde_json::Map::new();
    call.insert("index".into(), Value::from(0u64));
    call.insert("type".into(), Value::String("function".into()));
    // id 只随真调用下发；参数续包带 id 会被解码器当成新的 ToolCallStart。
    if !name.is_empty() {
        let id = function_call
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("call_wb_{}", uuid::Uuid::new_v4().simple()));
        call.insert("id".into(), Value::String(id));
    }
    call.insert("function".into(), Value::Object(function));
    let call = Value::Object(call);
    match delta.get_mut("tool_calls") {
        Some(Value::Array(calls)) => calls.push(call),
        _ => {
            delta.insert("tool_calls".into(), Value::Array(vec![call]));
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interpreter() -> StreamInterpreter {
        let endpoint = common::endpoint(PROTOCOL).expect("chat endpoint registered");
        StreamInterpreter::new(endpoint).expect("stream interpreter")
    }

    fn decode_all(interpreter: &mut StreamInterpreter, chunks: &[&[u8]]) -> Vec<AiStreamDelta> {
        let mut deltas = Vec::new();
        for &chunk in chunks {
            deltas.extend(interpreter.push(chunk).expect("decode chunk"));
        }
        deltas.extend(interpreter.finish().expect("finish stream"));
        deltas
    }

    fn text_of(deltas: &[AiStreamDelta]) -> String {
        deltas
            .iter()
            .filter_map(|delta| match delta {
                AiStreamDelta::TextDelta(text) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn split_utf8_codepoint_across_chunks() {
        // “你” 占 3 字节；把 TCP 块切在它的中间，整条流仍须正确重组。
        let sse = "data: {\"id\":\"r1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"你好\"}}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let bytes = sse.as_bytes();
        let cut = sse.find('你').expect("fixture") + 2;
        let mut interpreter = interpreter();
        let deltas = decode_all(&mut interpreter, &[&bytes[..cut], &bytes[cut..]]);
        assert_eq!(text_of(&deltas), "你好");
        assert!(
            deltas
                .iter()
                .any(|delta| matches!(delta, AiStreamDelta::Done { .. }))
        );
        assert!(interpreter.terminated);
    }

    #[test]
    fn duplicated_data_prefixes_and_heartbeats() {
        // 重复 `data:` 前缀、`: ` 心跳、`data: : x` 噪声、\r\n 边界都要吞掉。
        let sse = ": keepalive\r\n\r\ndata: data: {\"id\":\"r\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\ndata: : ping\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let mut interpreter = interpreter();
        let deltas = decode_all(&mut interpreter, &[sse.as_bytes()]);
        assert_eq!(text_of(&deltas), "hi");
        assert!(interpreter.terminated);
    }

    #[test]
    fn function_call_placeholder_vs_real_arguments() {
        // name+arguments 全空 = 占位噪声；name 非空 = 真调用；
        // 仅 arguments = 参数续包，只发 ToolCallDelta 不发 Start。
        let frames = [
            json!({"id":"r","model":"m","choices":[{"index":0,"delta":{"function_call":{"name":"lookup","arguments":"{\"q\":"}}}]}),
            json!({"choices":[{"index":0,"delta":{"function_call":{"name":"","arguments":"\"x\"}"}}}]}),
            json!({"choices":[{"index":0,"delta":{"function_call":{"name":"","arguments":""}}}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
        ];
        let mut sse = frames
            .iter()
            .map(|frame| format!("data: {frame}\n\n"))
            .collect::<String>();
        sse.push_str("data: [DONE]\n\n");
        let mut interpreter = interpreter();
        let deltas = decode_all(&mut interpreter, &[sse.as_bytes()]);
        let tool_deltas: Vec<&AiStreamDelta> = deltas
            .iter()
            .filter(|delta| {
                matches!(
                    delta,
                    AiStreamDelta::ToolCallStart { .. } | AiStreamDelta::ToolCallDelta { .. }
                )
            })
            .collect();
        assert_eq!(tool_deltas.len(), 3);
        assert!(matches!(
            tool_deltas[0],
            AiStreamDelta::ToolCallStart { index, name, .. }
                if *index == 0 && name == "lookup"
        ));
        assert!(matches!(
            tool_deltas[1],
            AiStreamDelta::ToolCallDelta { index, arguments }
                if *index == 0 && arguments == "{\"q\":"
        ));
        assert!(matches!(
            tool_deltas[2],
            AiStreamDelta::ToolCallDelta { index, arguments }
                if *index == 0 && arguments == "\"x\"}"
        ));
        assert!(interpreter.terminated);
    }

    #[test]
    fn missing_terminal_marker_is_rejected() {
        // 只有内容、没有 finish_reason 也没有 [DONE] 的截断流：
        // terminated 不成立，decode_stream 会以 UnexpectedEof 失败收尾。
        let sse = "data: {\"id\":\"r\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\n";
        let mut interpreter = interpreter();
        let deltas = decode_all(&mut interpreter, &[sse.as_bytes()]);
        assert!(!interpreter.terminated);
        assert!(
            !deltas
                .iter()
                .any(|delta| matches!(delta, AiStreamDelta::Done { .. }))
        );
    }

    #[test]
    fn in_band_error_frame_fails() {
        // 网关业务错误包 {"code":N} 要落成 StreamError，而不是被静默吞掉。
        let sse = "data: {\"code\":6004,\"message\":\"usage exceeds frequency limit\"}\n\n";
        let mut interpreter = interpreter();
        let deltas = decode_all(&mut interpreter, &[sse.as_bytes()]);
        assert!(
            deltas
                .iter()
                .any(|delta| matches!(delta, AiStreamDelta::StreamError { .. }))
        );
    }

    #[test]
    fn bare_done_is_not_a_completed_response() {
        let mut interpreter = interpreter();
        decode_all(&mut interpreter, &[b"data: [DONE]\n\n"]);
        assert!(!interpreter.terminated);
    }

    #[test]
    fn cli_body_filters_structure_without_changing_user_data_or_signatures() {
        let schema = json!({"type":"object", "properties":{"x_extra":{"default":{"x_extra":1}}}, "x_schema":true});
        let mut body = json!({
            "model":"test-model", "metadata":{"x_extra":true},
            "thinking":{"type":"enabled", "clear_thinking":false, "x_extra":true},
            "reasoning":{"effort":"high", "enabled":true, "x_extra":true},
            "chat_template_kwargs":{"enable_thinking":true, "preserve_thinking":true, "x_extra":true},
            "messages":[{
                "role":"assistant", "x_extra":true,
                "content":[{"type":"text", "text":"x_extra stays in text", "x_extra":true,
                    "cache_control":{"type":"ephemeral", "x_extra":true}}],
                "extra_fields":{"_meta":{"x_extra":"user metadata"}, "x_extra":true,
                    "google":{"thought_signature":"opaque-signed-state", "x_extra":true}},
                "tool_calls":[{"id":"call_1", "type":"function", "x_extra":true,
                    "function":{"name":"lookup", "arguments":"{\"x_extra\":true}", "x_extra":true},
                    "extra_content":{"google":{"thought_signature":"tool-signed-state", "x_extra":true}, "x_extra":true}}]
            }],
            "tools":[{"type":"function", "x_extra":true, "function":{
                "name":"lookup", "parameters":schema, "strict":true, "x_extra":true}}],
            "response_format":{"type":"json_schema", "x_extra":true,
                "json_schema":{"name":"answer", "strict":true, "schema":schema, "x_extra":true}}
        });
        retain_cli_body(&mut body);
        assert!(body.get("metadata").is_none());
        assert_eq!(
            body["thinking"],
            json!({"type":"enabled", "clear_thinking":false})
        );
        assert_eq!(body["reasoning"], json!({"effort":"high", "enabled":true}));
        assert_eq!(
            body["chat_template_kwargs"],
            json!({"enable_thinking":true, "preserve_thinking":true})
        );
        let message = &body["messages"][0];
        assert!(message.get("x_extra").is_none());
        assert_eq!(
            message["content"][0],
            json!({"type":"text", "text":"x_extra stays in text", "cache_control":{"type":"ephemeral"}})
        );
        assert_eq!(
            message["extra_fields"],
            json!({"_meta":{"x_extra":"user metadata"}, "google":{"thought_signature":"opaque-signed-state"}})
        );
        assert_eq!(
            message["tool_calls"][0],
            json!({"id":"call_1", "type":"function",
            "function":{"name":"lookup", "arguments":"{\"x_extra\":true}"},
            "extra_content":{"google":{"thought_signature":"tool-signed-state"}}})
        );
        assert_eq!(
            body["tools"][0],
            json!({"type":"function", "function":{"name":"lookup", "parameters":schema, "strict":true}})
        );
        assert_eq!(
            body["response_format"],
            json!({"type":"json_schema", "json_schema":{"name":"answer", "strict":true, "schema":schema}})
        );
    }

    #[test]
    fn cli_headers_ignore_unknown_names_case_insensitively() {
        let mut headers = vec![
            ("AuThOrIzAtIoN".into(), "Bearer local-test".into()),
            ("X-Requested-With".into(), "XMLHttpRequest".into()),
            ("X-Client-Only".into(), "ignored".into()),
            ("oRiGiN".into(), "https://client.invalid".into()),
            ("X-Product-Version".into(), "ignored".into()),
        ];
        retain_cli_headers(&mut headers);
        assert_eq!(
            headers,
            vec![
                ("AuThOrIzAtIoN".to_owned(), "Bearer local-test".to_owned()),
                ("X-Requested-With".to_owned(), "XMLHttpRequest".to_owned()),
            ]
        );
    }

    fn metadata(extensions: &[(&str, Value)]) -> ModelMetadata {
        ModelMetadata {
            id: None,
            family: None,
            selector: None,
            capabilities: Vec::new(),
            extensions: extensions
                .iter()
                .map(|(key, value)| ((*key).to_owned(), value.clone()))
                .collect(),
        }
    }

    #[test]
    fn deepseek_default_enables_thinking_with_fallback_effort() {
        // 什么都没指定时按官方形态注入：思考标记 + 兜底档 + summary。
        let mut body = json!({"model": "deepseek-v4.1-flash"});
        assert!(serialize_deepseek_thinking(&mut body, None));
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["reasoning_summary"], "auto");
    }

    #[test]
    fn deepseek_client_effort_is_preserved() {
        let mut body = json!({
            "model": "deepseek-v4.1-flash",
            "reasoning_effort": "low",
        });
        assert!(serialize_deepseek_thinking(&mut body, None));
        assert_eq!(body["reasoning_effort"], "low");
        assert_eq!(body["thinking"]["type"], "enabled");
    }

    #[test]
    fn deepseek_catalog_default_effort_wins_over_fallback() {
        // 目录声明 defaultEffort 的模型，缺省注入取目录值而非 "high"。
        let meta = metadata(&[("reasoning_default_effort", json!("medium"))]);
        let mut body = json!({"model": "deepseek-v3-2-volc"});
        assert!(serialize_deepseek_thinking(&mut body, Some(&meta)));
        assert_eq!(body["reasoning_effort"], "medium");
    }

    #[test]
    fn deepseek_effort_none_serializes_as_disabled() {
        // 宿主 off 档映射为 reasoning_effort="none"：官方形态是
        // thinking:{type:"disabled"} 且删除 effort 字段。
        let mut body = json!({
            "model": "deepseek-v4.1-flash",
            "reasoning_effort": "none",
        });
        assert!(!serialize_deepseek_thinking(&mut body, None));
        assert_eq!(body["thinking"]["type"], "disabled");
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("reasoning_summary").is_none());
    }

    #[test]
    fn non_deepseek_effort_none_is_dropped_but_real_effort_kept() {
        let mut body = json!({
            "model": "glm-4.6",
            "reasoning_effort": "none",
            "messages": [{"role": "user", "content": "hi"}],
        });
        normalize_request_body(&mut body, None);
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("thinking").is_none());

        let mut body = json!({
            "model": "glm-5.3",
            "reasoning_effort": "high",
            "messages": [{"role": "user", "content": "hi"}],
        });
        normalize_request_body(&mut body, None);
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn deepseek_disabled_type_drops_effort() {
        let mut body = json!({
            "model": "deepseek-v4.1-flash",
            "thinking": {"type": "disabled"},
            "reasoning_effort": "high",
        });
        assert!(!serialize_deepseek_thinking(&mut body, None));
        assert_eq!(body["thinking"]["type"], "disabled");
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn deepseek_camel_effort_merges_into_snake() {
        let mut body = json!({
            "model": "deepseek-v4.1-flash",
            "reasoningEffort": "max",
        });
        assert!(serialize_deepseek_thinking(&mut body, None));
        assert_eq!(body["reasoning_effort"], "max");
        assert!(body.get("reasoningEffort").is_none());
    }

    #[test]
    fn deepseek_non_reasoning_model_gets_no_injection() {
        // 目录标注 supportsReasoning=false：不造默认档；无显式意图时
        // 返回 false，历史回填交给 has_trace 门槛。
        let meta = metadata(&[("reasoning", json!(false))]);
        let mut body = json!({"model": "deepseek-v3-0324"});
        assert!(!serialize_deepseek_thinking(&mut body, Some(&meta)));
        assert!(body.get("thinking").is_none());
        assert!(body.get("reasoning_effort").is_none());
        // 显式开启仍尊重客户端。
        let mut body = json!({
            "model": "deepseek-v3-0324",
            "reasoning_effort": "low",
        });
        assert!(serialize_deepseek_thinking(&mut body, Some(&meta)));
        assert_eq!(body["reasoning_effort"], "low");
    }

    fn session_provider(client_headers: &[(&str, &str)]) -> ProviderSnapshot {
        ProviderSnapshot {
            provider_id: crate::VENDOR_ID.into(),
            channel: "cn".into(),
            base_url: Region::Cn.origin().into(),
            protocol: PROTOCOL.into(),
            options: Default::default(),
            credentials: Default::default(),
            model: Some("hy4-preview".into()),
            model_metadata: None,
            client_headers: client_headers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            operation_metadata: Default::default(),
        }
    }

    fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn conversation_id_uses_host_session_affinity() {
        let affinity = "0123456789abcdef".repeat(4);
        let mut provider = session_provider(&[("session_id", "client-session")]);
        provider
            .operation_metadata
            .insert("session_affinity".into(), Value::String(affinity.clone()));
        let mut headers = Vec::new();
        apply_session_headers(&mut headers, &provider);
        assert_eq!(
            header_value(&headers, "x-conversation-id"),
            Some(affinity.as_str())
        );
        assert!(
            header_value(&headers, "baggage")
                .is_some_and(|v| v.starts_with(&format!("codebuddy.session_id={affinity},")))
        );
    }

    #[test]
    fn conversation_id_absent_without_host_affinity() {
        // 客户端自报的 session 不作为上游键；宿主未下发时也不伪造——
        // 随机会话键只会打散上游亲和视图。
        let provider = session_provider(&[("session_id", "client-session")]);
        let mut headers = Vec::new();
        apply_session_headers(&mut headers, &provider);
        assert!(header_value(&headers, "x-conversation-id").is_none());
        assert!(header_value(&headers, "baggage").is_none());
        // 轮次级与追踪标头仍然齐备。
        assert!(header_value(&headers, "x-conversation-message-id").is_some());
        assert!(header_value(&headers, "x-conversation-request-id").is_some());
    }

    #[test]
    fn request_id_matches_message_id_like_the_official_client() {
        let provider = session_provider(&[]);
        let mut headers = Vec::new();
        apply_session_headers(&mut headers, &provider);
        let message = header_value(&headers, "x-conversation-message-id").unwrap();
        assert_eq!(header_value(&headers, "x-request-id"), Some(message));
    }

    #[test]
    fn trace_headers_form_a_valid_w3c_pair() {
        let provider = session_provider(&[]);
        let mut headers = Vec::new();
        apply_session_headers(&mut headers, &provider);
        let traceparent = header_value(&headers, "traceparent").unwrap();
        let trace_id = header_value(&headers, "x-trace-id").unwrap();
        assert_eq!(trace_id.len(), 32);
        assert!(trace_id.bytes().all(|b| b.is_ascii_hexdigit()));
        let parts: Vec<&str> = traceparent.split('-').collect();
        assert_eq!(parts.as_slice(), ["00", trace_id, parts[2], "01"]);
        assert_eq!(parts[2].len(), 16);
        assert_eq!(header_value(&headers, "x-b3-traceid"), Some(trace_id));
        assert_eq!(header_value(&headers, "x-b3-spanid"), Some(parts[2]));
    }

    #[test]
    fn generated_legacy_call_ids_do_not_collide_across_turns() {
        let frame = json!({"choices":[{"index":0,"delta":{"function_call":{"name":"lookup","arguments":"{}"}},"finish_reason":"function_call"}]});
        let sse = format!("data: {frame}\n\ndata: [DONE]\n\n");
        let mut ids = Vec::new();
        for _ in 0..2 {
            let mut interpreter = interpreter();
            for delta in decode_all(&mut interpreter, &[sse.as_bytes()]) {
                if let AiStreamDelta::ToolCallStart { id, .. } = delta {
                    ids.push(id);
                }
            }
        }
        assert_ne!(
            ids[0], ids[1],
            "conversation history requires distinct tool-call IDs"
        );
    }
}
