use std::{borrow::Cow, collections::BTreeMap, fmt::Write as _};

use opentelemetry::{
    Context,
    propagation::{Extractor, Injector, TextMapPropagator},
    trace::{SpanContext, SpanId, TraceContextExt, TraceFlags, TraceState},
};
use opentelemetry_propagator_b3::{B3Encoding, Propagator as B3Propagator};
use opentelemetry_sdk::{
    propagation::TraceContextPropagator,
    trace::{IdGenerator, RandomIdGenerator},
};
use serde_json::Value;
use stravia_vendor_sdk::{GuestHost, LogLevel, ProviderSnapshot};

/// 用户运行的 root 与父上下文由宿主提供；模型 span 跨内部重试，HTTP span 每次新建。
pub(crate) struct RequestContext<'a> {
    root_request_id: Cow<'a, str>,
    conversation_request_id: String,
    conversation_id: Option<&'a str>,
    acp_connection_id: Option<&'a str>,
    model_span: SpanContext,
    parent_span_id: Option<SpanId>,
}

impl<'a> RequestContext<'a> {
    pub(crate) fn new(host: &GuestHost, provider: &'a ProviderSnapshot) -> Self {
        let metadata = &provider.operation_metadata;
        let root_request_id = metadata_header(host, metadata, "root_request_id")
            .map(Cow::Borrowed)
            .unwrap_or_else(|| {
                host.log(
                    LogLevel::Warn,
                    "WorkBuddy host root_request_id is unavailable; using an inference-scoped root",
                );
                Cow::Owned(uuid::Uuid::new_v4().simple().to_string())
            });
        let parent_context = TraceContextPropagator::new().extract(&MetadataCarrier(metadata));
        let parent = parent_context.span().span_context().clone();
        let ids = RandomIdGenerator::default();
        let (trace_id, flags, state, parent_span_id) = if parent.is_valid() {
            (
                parent.trace_id(),
                parent.trace_flags(),
                parent.trace_state().clone(),
                Some(parent.span_id()),
            )
        } else {
            host.log(
                LogLevel::Warn,
                "WorkBuddy host trace context is unavailable or invalid; starting an inference-scoped non-recording trace",
            );
            (
                ids.new_trace_id(),
                TraceFlags::NOT_SAMPLED,
                TraceState::default(),
                None,
            )
        };
        let acp_connection_id = metadata_header(host, metadata, "acp_connection_id");
        if acp_connection_id.is_none() {
            host.log(
                LogLevel::Info,
                "WorkBuddy direct inference has no ACP connection; acp-connection-id omitted",
            );
        }
        Self {
            root_request_id,
            conversation_request_id: uuid::Uuid::new_v4().simple().to_string(),
            conversation_id: metadata_header(host, metadata, "session_affinity"),
            acp_connection_id,
            model_span: SpanContext::new(trace_id, ids.new_span_id(), flags, false, state),
            parent_span_id,
        }
    }

    pub(crate) fn apply(&self, headers: &mut Vec<(String, String)>) {
        let message_id = uuid::Uuid::new_v4().simple().to_string();
        headers.push(("x-request-id".into(), message_id.clone()));
        headers.push(("x-conversation-message-id".into(), message_id));
        headers.push(("x-root-request-id".into(), self.root_request_id.to_string()));
        headers.push((
            "x-conversation-request-id".into(),
            self.conversation_request_id.clone(),
        ));
        if let Some(conversation_id) = self.conversation_id {
            headers.push(("x-conversation-id".into(), conversation_id.to_owned()));
            let mut baggage = String::with_capacity(
                64 + conversation_id.len() + self.conversation_request_id.len(),
            );
            baggage.push_str("codebuddy.session_id=");
            baggage.extend(url::form_urlencoded::byte_serialize(
                conversation_id.as_bytes(),
            ));
            baggage.push_str(",codebuddy.conversation_request_id=");
            baggage.push_str(&self.conversation_request_id);
            headers.push(("baggage".into(), baggage));
        }
        if let Some(connection_id) = self.acp_connection_id {
            headers.push(("acp-connection-id".into(), connection_id.to_owned()));
        }
        headers.push(("x-trace-id".into(), self.model_span.trace_id().to_string()));
        let model_context = Context::new().with_remote_span_context(self.model_span.clone());
        B3Propagator::with_encoding(B3Encoding::SingleAndMultiHeader)
            .inject_context(&model_context, &mut HeaderCarrier(headers));
        // OTel 标准不注入 B3 parent；WorkBuddy 的模型请求扩展仍使用这两个字段。
        if let Some(parent) = self.parent_span_id {
            headers.push(("x-b3-parentspanid".into(), parent.to_string()));
            if let Some((_, b3)) = headers.iter_mut().find(|(key, _)| key == "b3") {
                write!(b3, "-{parent}").expect("String formatting cannot fail");
            }
        }
        // W3C 表示本次 HTTP 子 span；B3 保留跨内部重试的模型 span。
        let http_span = SpanContext::new(
            self.model_span.trace_id(),
            RandomIdGenerator::default().new_span_id(),
            self.model_span.trace_flags(),
            false,
            self.model_span.trace_state().clone(),
        );
        let http_context = Context::new().with_remote_span_context(http_span);
        TraceContextPropagator::new().inject_context(&http_context, &mut HeaderCarrier(headers));
    }
}

fn metadata_header<'a>(
    host: &GuestHost,
    metadata: &'a BTreeMap<String, Value>,
    key: &str,
) -> Option<&'a str> {
    let value = metadata.get(key)?;
    if let Some(value) = value.as_str().filter(|value| {
        !value.is_empty() && value.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
    }) {
        return Some(value);
    }
    host.log(
        LogLevel::Warn,
        &format!("WorkBuddy unsafe host metadata `{key}` omitted"),
    );
    None
}

struct MetadataCarrier<'a>(&'a BTreeMap<String, Value>);

impl Extractor for MetadataCarrier<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(Value::as_str)
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(String::as_str).collect()
    }
}

struct HeaderCarrier<'a>(&'a mut Vec<(String, String)>);

impl Injector for HeaderCarrier<'_> {
    fn set(&mut self, key: &str, value: String) {
        if key != "tracestate" || !value.is_empty() {
            self.0.push((key.to_owned(), value));
        }
    }
}
