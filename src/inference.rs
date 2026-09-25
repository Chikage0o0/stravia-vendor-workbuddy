//! WorkBuddy `/v2/chat/completions` 推理通道。
//!
//! 请求由共享编解码器产出 canonical 请求体后，只做上游线规要求的最小修正；
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
    AiErrorKind, AiStreamDelta, ErrorKind, GuestHost, HttpRequest, HttpResponse, OperationOutput,
    PluginError, ProviderSnapshot, read_http_body,
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
    request.model = model.to_owned();
    // 上游对话接口只提供 SSE 流式响应；include_usage 由编解码器默认补齐。
    request.stream.enabled = true;
    // 凭据中的 region/domain 与请求区域不一致时，auth::headers 会在任何
    // HTTP 发生前直接失败。
    let auth_headers = auth::headers(provider, region)?;
    let encoded = common::encode_inference_request(PROTOCOL, &request)?;
    let mut body = encoded.body;
    normalize_request_body(&mut body);
    let body = serde_json::to_vec(&body).map_err(|error| {
        common::plugin_error(
            ErrorKind::Invalid,
            format!("failed to serialize request body: {error}"),
        )
    })?;
    let mut headers = common::header_pairs(&encoded.headers)?;
    headers.extend(auth_headers);
    set_header(&mut headers, "content-type", "application/json");
    set_header(&mut headers, "accept", "text/event-stream");
    set_header(&mut headers, "x-model-id", model);
    host.emit_started()?;
    let response = host.http_start(HttpRequest {
        method: "POST".into(),
        url: format!("{}{CHAT_PATH}", region.origin()),
        headers,
        body,
    })?;
    decode_response(host, response)
}

fn set_header(headers: &mut Vec<(String, String)>, name: &str, value: &str) {
    headers.retain(|(key, _)| !key.eq_ignore_ascii_case(name));
    headers.push((name.to_owned(), value.to_owned()));
}

/// 编码后请求体的线规修正，逐项对应上游校验错误码：
/// developer→system（11128）、首条 system 兜底、tool_choice 归一（11101）、
/// max_completion_tokens 别名、deepseek 思考模式 reasoning 回填（11155）。
/// 不做指纹清洗，不改写用户提示词。
fn normalize_request_body(body: &mut Value) {
    normalize_tool_choice(body);
    translate_max_completion_tokens(body);
    let is_deepseek = body
        .get("model")
        .and_then(Value::as_str)
        .is_some_and(|model| model.to_ascii_lowercase().starts_with("deepseek"));
    let thinking_enabled = deepseek_thinking_enabled(body);
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

/// deepseek 系模型在 `thinking.type` 非 disabled 且 effort 非 none
/// 时视为思考开启；thinking.enabled 恒为开。
fn deepseek_thinking_enabled(body: &Value) -> bool {
    let model = body.get("model").and_then(Value::as_str).unwrap_or("");
    if !model.to_ascii_lowercase().starts_with("deepseek") {
        return false;
    }
    let thinking_type = body
        .get("thinking")
        .and_then(|thinking| thinking.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if thinking_type.eq_ignore_ascii_case("enabled") {
        return true;
    }
    if thinking_type.eq_ignore_ascii_case("disabled") {
        return false;
    }
    let effort = body
        .get("reasoning_effort")
        .or_else(|| body.get("reasoningEffort"))
        .and_then(Value::as_str)
        .unwrap_or("");
    !effort.trim().eq_ignore_ascii_case("none")
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

fn decode_response(
    host: &GuestHost,
    response: HttpResponse,
) -> Result<OperationOutput, PluginError> {
    let status = response.status()?;
    let response_headers = response.headers()?;
    if !(200..300).contains(&status) {
        let body = read_http_body(&response, MAX_ERROR_BODY)?;
        let mut error = common::upstream_error(status, &response_headers, &body);
        error.message = format!("WorkBuddy inference HTTP {status}");
        return Err(error);
    }
    if is_event_stream(&response_headers) {
        decode_stream(host, response)
    } else {
        // 网关始终回 SSE；非流式响应仍按共享一元解码兜底。
        decode_unary(host, response)
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

fn decode_unary(host: &GuestHost, response: HttpResponse) -> Result<OperationOutput, PluginError> {
    let body = read_http_body(&response, MAX_UNARY_BODY)?;
    let value: Value = serde_json::from_slice(&body).map_err(|error| {
        common::model_error(
            AiErrorKind::ServerError,
            format!("upstream returned invalid JSON: {error}"),
        )
    })?;
    if let Some(error) = in_band_error(&value) {
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

fn decode_stream(host: &GuestHost, response: HttpResponse) -> Result<OperationOutput, PluginError> {
    let endpoint = common::endpoint(PROTOCOL)?;
    let mut interpreter = StreamInterpreter::new(endpoint)?;
    let mut accumulator = StreamResponseAccumulator::default();
    while let Some(chunk) = response.read_body()? {
        let deltas = interpreter.push(&chunk)?;
        common::emit_deltas(host, &mut accumulator, &deltas)?;
    }
    let deltas = interpreter.finish()?;
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
        {
            return Cow::Borrowed(payload);
        }
        let Ok(text) = std::str::from_utf8(payload) else {
            return Cow::Borrowed(payload);
        };
        let Ok(mut value) = serde_json::from_str::<Value>(text) else {
            return Cow::Borrowed(payload);
        };
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
