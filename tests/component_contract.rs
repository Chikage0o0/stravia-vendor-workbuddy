use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use parking_lot::Mutex;
use serde_json::{Value, json};
use stravia_protocol_codec::transform::ProtocolTransform;
use stravia_runtime_contract::{CancellationToken, Deadline};
use stravia_vendor_common::common;
use stravia_vendor_runtime::{
    HostFailure, HostHttpResponse, HostServices, HostWebSocket, HttpRequest, LoadedPlugin,
    LogLevel, OperationScope, RuntimeError, RuntimeEvent, VendorRuntime, WsRequest,
};
use stravia_vendor_sdk::{
    AllowanceRequest, AuthRequest, AuthResponse, AuthStep, ConnectionCheckRequest,
    ConnectionCheckVerification, DiscoverRequest, ErrorKind, HttpContentCoding, OperationInput,
    OperationOutput, ProviderSnapshot,
};

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    chunks: Mutex<VecDeque<Vec<u8>>>,
}

impl Reply {
    fn json(body: Value) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type".into(), "application/json".into())],
            chunks: Mutex::new(VecDeque::from([serde_json::to_vec(&body).unwrap()])),
        }
    }
    fn stream(body: &str) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            chunks: Mutex::new(body.as_bytes().chunks(7).map(<[u8]>::to_vec).collect()),
        }
    }
}

#[async_trait]
impl HostHttpResponse for Reply {
    async fn status(&self) -> Result<u16, HostFailure> {
        Ok(self.status)
    }
    async fn headers(&self) -> Result<Vec<(String, String)>, HostFailure> {
        Ok(self.headers.clone())
    }
    async fn read_body(&self) -> Result<Option<Vec<u8>>, HostFailure> {
        Ok(self.chunks.lock().pop_front())
    }
}

struct LocalUpstream {
    origins: Vec<String>,
    replies: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<HttpRequest>>,
    state: Mutex<Option<Vec<u8>>>,
    events: Mutex<Vec<RuntimeEvent>>,
}

impl LocalUpstream {
    fn new(origins: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            origins: origins.iter().map(|origin| (*origin).into()).collect(),
            replies: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Vec::new()),
            state: Mutex::new(None),
            events: Mutex::new(Vec::new()),
        })
    }
    fn reply(&self, value: Reply) {
        self.replies.lock().push_back(value);
    }
    fn scope(self: &Arc<Self>) -> OperationScope {
        OperationScope::new(
            self.clone(),
            CancellationToken::new(),
            Deadline::from_now(Duration::from_secs(30)),
            1,
        )
    }
}

#[async_trait]
impl HostServices for LocalUpstream {
    fn http_start(&self, request: HttpRequest) -> Result<Arc<dyn HostHttpResponse>, HostFailure> {
        let url = url::Url::parse(&request.url)
            .map_err(|_| HostFailure::new(ErrorKind::Invalid, "invalid HTTP destination"))?;
        if !self.origins.contains(&url.origin().ascii_serialization()) {
            return Err(HostFailure::new(
                ErrorKind::Invalid,
                "HTTP destination must be declared and approved",
            ));
        }
        if url.path() == "/v2/chat/completions" {
            let _ = application_request_body(&request);
        } else {
            assert!(
                request.compression.is_none(),
                "authentication, configuration, and allowance calls must retain legacy HTTP policy"
            );
            assert_no_compression_headers(&request);
        }
        self.requests.lock().push(request);
        Ok(Arc::new(
            self.replies
                .lock()
                .pop_front()
                .expect("unexpected HTTP request or retry"),
        ))
    }
    async fn ws_connect(&self, _: WsRequest) -> Result<Arc<dyn HostWebSocket>, HostFailure> {
        Err(HostFailure::new(
            ErrorKind::Unsupported,
            "WebSocket is not allowed in this contract",
        ))
    }
    async fn read_private_state(&self) -> Result<Option<Vec<u8>>, HostFailure> {
        Ok(self.state.lock().clone())
    }
    async fn write_private_state(&self, bytes: Vec<u8>) -> Result<(), HostFailure> {
        *self.state.lock() = Some(bytes);
        Ok(())
    }
    async fn emit_event(&self, event: RuntimeEvent) -> Result<(), HostFailure> {
        self.events.lock().push(event);
        Ok(())
    }
    fn log(&self, _: LogLevel, _: &str) {}
    fn generation_is_current(&self, generation: u64) -> bool {
        generation == 1
    }
}

async fn run(
    runtime: &VendorRuntime,
    plugin: &LoadedPlugin,
    channel: &str,
    input: OperationInput,
    scope: OperationScope,
) -> Result<OperationOutput, RuntimeError> {
    let (operation, encoded) = input
        .encode_for_host()
        .map_err(|_| RuntimeError::InvalidOutput)?;
    drop(input);
    runtime
        .execute(plugin, channel, operation, encoded, scope)
        .await
}

fn snapshot(region: &str, origin: &str) -> ProviderSnapshot {
    ProviderSnapshot {
        provider_id: "workbuddy".into(),
        channel: region.into(),
        base_url: origin.into(),
        protocol: "openai-compatible".into(),
        options: BTreeMap::new(),
        credentials: BTreeMap::new(),
        model: Some("test-model".into()),
        model_metadata: None,
        client_headers: Vec::new(),
        operation_metadata: BTreeMap::new(),
    }
}

fn token(domain: &str, uid: &str, nonce: u8) -> String {
    let claims = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({"sub":uid,"iss":format!("https://{domain}"),"exp":4102444800_i64,"nonce":nonce})).unwrap());
    format!("e30.{claims}.local-test-signature")
}

fn request() -> stravia_vendor_sdk::AiRequest {
    let endpoint = common::endpoint("openai-compatible").unwrap();
    ProtocolTransform::global().bind(endpoint, endpoint).unwrap().decode_request(json!({
        "model":"client-route-name", "stream":true,
        "messages":[{"role":"user","content":"你好"}],
        "tools":[{"type":"function","function":{"name":"weather","parameters":{"type":"object","properties":{"city":{"type":"string"}}}}}]
    })).unwrap()
}

fn response_wire(output: OperationOutput) -> Value {
    let OperationOutput::Infer(response) = output else {
        panic!("expected inference output")
    };
    let endpoint = common::endpoint("openai-compatible").unwrap();
    ProtocolTransform::global()
        .bind(endpoint, endpoint)
        .unwrap()
        .encode_response(&response)
        .unwrap()
}

fn request_header<'a>(request: &'a HttpRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn assert_no_chat_identity_headers(request: &HttpRequest) {
    assert!(
        request.headers.iter().all(|(name, _)| {
            !name
                .get(.."x-stainless-".len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("x-stainless-"))
        }),
        "SDK chat profile must not leak into discovery or allowance requests"
    );
    for name in [
        "x-userinfo",
        "x-private-data",
        "x-agent-intent",
        "x-agent-type",
        "x-agent-purpose",
    ] {
        assert!(
            request_header(request, name).is_none(),
            "{name} is chat-only and must not appear on discovery or allowance requests"
        );
    }
}

fn assert_no_compression_headers(request: &HttpRequest) {
    for name in ["accept-encoding", "content-encoding", "content-length"] {
        assert!(
            request_header(request, name).is_none(),
            "{name} must be owned by the host, not the guest"
        );
    }
}

fn application_request_body(request: &HttpRequest) -> Value {
    assert_no_compression_headers(request);
    let compression = request
        .compression
        .as_ref()
        .expect("chat must explicitly opt into host-owned compression, even for identity");
    assert!(
        compression.response.is_empty(),
        "chat responses must retain identity-only negotiation"
    );
    serde_json::from_slice(&request.body)
        .expect("the host must receive original application JSON bytes, not precompressed bytes")
}

fn assert_chat_compression(request: &HttpRequest, gzip: bool) {
    let _ = application_request_body(request);
    let coding = &request.compression.as_ref().unwrap().request;
    if gzip {
        assert!(matches!(coding, Some(HttpContentCoding::Gzip)));
    } else {
        assert!(
            coding.is_none(),
            "the official gzip switch must disable request coding"
        );
    }
}

fn whitelist_request() -> stravia_vendor_sdk::AiRequest {
    let endpoint = common::endpoint("openai-compatible").unwrap();
    let mut request = ProtocolTransform::global()
        .bind(endpoint, endpoint)
        .unwrap()
        .decode_request(json!({
            "model":"client-route-name", "stream":true,
            "temperature":0.25,"top_p":0.8,"max_tokens":123,"reasoning_effort":"high",
            "store":true,"prompt_cache_retention":"24h","prompt_cache_key":"client-only-cache-key",
            "logit_bias":{"1":1},"logprobs":true,"top_logprobs":2,"n":2,"seed":7,
            "user":"client-user","metadata":{"client":"only"},"service_tier":"auto",
            "stream_options":{"include_usage":true,"include_obfuscation":true},
            "response_format":{"type":"json_schema","json_schema":{
                "name":"weather_result","schema":{
                    "type":"object","properties":{"x_client_only":{"type":"string","default":"杭州"}},
                    "default":{"x_client_only":"杭州"},"x_schema_extension":{"x_structure_only":true}
                }
            }},
            "x_client_only":{"secret":"never-forward"},
            "thinking":{"type":"enabled","x_structure_only":true},
            "messages":[
                {"role":"user","content":"保留 x_client_only 和 x_structure_only 原文","x_structure_only":true},
                {"role":"assistant","content":null,"tool_calls":[
                    {"id":"call_weather","type":"function","x_structure_only":true,
                     "function":{"name":"weather","arguments":"{\"x_client_only\":{\"city\":\"杭州\"}}","x_structure_only":true}}
                ]},
                {"role":"tool","tool_call_id":"call_weather","content":"{\"x_client_only\":\"tool result\"}","x_structure_only":true}
            ],
            "tools":[{"type":"function","x_structure_only":true,"function":{
                "name":"weather","description":"保留 x_structure_only 描述","x_structure_only":true,
                "parameters":{
                    "type":"object",
                    "properties":{"x_client_only":{"type":"object","default":{"x_structure_only":"杭州"}}},
                    "default":{"x_client_only":{"x_structure_only":"杭州"}},
                    "x_schema_extension":{"x_structure_only":true}
                }
            }}]
        }))
        .unwrap();
    // 宿主路由后的目标控制必须覆盖入口 high；统一参数不能绕过此优先级。
    request.reasoning.target_control = Some(
        stravia_runtime_contract::thinking::TargetThinkingControl::Effort {
            value: "medium".into(),
        },
    );
    if let Some(raw) = request.meta.raw.as_mut() {
        raw.headers
            .insert("X-Client-Only".into(), "never-forward".into());
        raw.headers
            .insert("aUtHoRiZaTiOn".into(), "client-auth-must-not-win".into());
    }
    request
}

fn request_with_unknown() -> stravia_vendor_sdk::AiRequest {
    let endpoint = common::endpoint("openai-compatible").unwrap();
    let mut wire = ProtocolTransform::global()
        .bind(endpoint, endpoint)
        .unwrap()
        .encode_request(request())
        .unwrap()
        .body;
    wire["x_client_only"] = json!({"secret":"never-forward"});
    ProtocolTransform::global()
        .bind(endpoint, endpoint)
        .unwrap()
        .decode_request(wire)
        .unwrap()
}

#[tokio::test]
#[ignore = "build the release wasm32-wasip2 component first"]
async fn inference_whitelist_preserves_user_payload_contract() {
    let artifact = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/wasm32-wasip2/release/stravia_vendor_workbuddy.wasm");
    let runtime = VendorRuntime::new().unwrap();
    let plugin = runtime
        .load(&std::fs::read(artifact).unwrap())
        .await
        .unwrap();
    for (region, domain) in [("cn", "www.workbuddy.cn"), ("intl", "www.workbuddy.ai")] {
        let origin = format!("https://{domain}");
        let local = LocalUpstream::new(&[&origin]);
        let mut provider = snapshot(region, &origin);
        let access_token = token(domain, "whitelist-account", 1);
        provider.credentials = BTreeMap::from([
            ("region".into(), json!(region)),
            ("domain".into(), json!(domain)),
            ("uid".into(), json!("whitelist-account")),
            ("access_token".into(), json!(access_token)),
        ]);
        provider.client_headers = vec![
            ("x-ClIeNt-OnLy".into(), "never-forward".into()),
            ("aUtHoRiZaTiOn".into(), "client-auth-must-not-win".into()),
            ("X-Domain".into(), "attacker.invalid".into()),
            ("X-Model-Id".into(), "attacker-model".into()),
            ("X-Product-Version".into(), "attacker-version".into()),
            ("Origin".into(), "https://attacker.invalid".into()),
            ("Referer".into(), "https://attacker.invalid".into()),
            ("X-CodeBuddy-Request".into(), "attacker".into()),
            ("Accept-Language".into(), "attacker".into()),
        ];
        let input = whitelist_request();
        let endpoint = common::endpoint("openai-compatible").unwrap();
        let original = ProtocolTransform::global()
            .bind(endpoint, endpoint)
            .unwrap()
            .encode_request(input.clone())
            .unwrap()
            .body;
        local.reply(Reply::stream(&completion_stream(
            "test-model",
            "whitelist answer",
        )));
        let output = run(
            &runtime,
            &plugin,
            region,
            OperationInput::Infer {
                provider,
                request: input,
            },
            local.scope(),
        )
        .await
        .unwrap();
        let wire = response_wire(output);
        assert_eq!(wire["choices"][0]["message"]["content"], "whitelist answer");
        let requests = local.requests.lock();
        assert_eq!(requests.len(), 1);
        let chat = &requests[0];
        assert_eq!(chat.url, format!("{origin}/v2/chat/completions"));
        assert_chat_compression(chat, true);
        let body = application_request_body(chat);
        assert!(body.get("x_client_only").is_none());
        for key in [
            "logit_bias",
            "logprobs",
            "top_logprobs",
            "n",
            "seed",
            "user",
            "metadata",
            "service_tier",
            "prompt_cache_key",
        ] {
            assert!(body.get(key).is_none(), "{key} must not be forwarded");
        }
        assert_eq!(body["temperature"], json!(0.25));
        assert_eq!(body["top_p"], json!(0.8));
        assert_eq!(body["max_tokens"], json!(123));
        assert_eq!(body["reasoning_effort"], "medium");
        assert_eq!(body["store"], true);
        assert_eq!(body["prompt_cache_retention"], "24h");
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert!(body["stream_options"].get("include_obfuscation").is_none());
        assert_eq!(
            body["response_format"]["json_schema"]["schema"],
            json!({
                "type":"object","properties":{"x_client_only":{"type":"string","default":"杭州"}},
                "default":{"x_client_only":"杭州"},"x_schema_extension":{"x_structure_only":true}
            })
        );
        assert!(body["thinking"].get("x_structure_only").is_none());
        for message in body["messages"].as_array().unwrap() {
            assert!(message.get("x_structure_only").is_none());
            if let Some(calls) = message["tool_calls"].as_array() {
                for call in calls {
                    assert!(call.get("x_structure_only").is_none());
                    assert!(call["function"].get("x_structure_only").is_none());
                    assert_eq!(
                        call["function"]["arguments"],
                        "{\"x_client_only\":{\"city\":\"杭州\"}}"
                    );
                }
            }
        }
        let messages = body["messages"].as_array().unwrap();
        let user = messages
            .iter()
            .find(|message| message["role"] == "user")
            .unwrap();
        let tool = messages
            .iter()
            .find(|message| message["role"] == "tool")
            .unwrap();
        let assistant = messages
            .iter()
            .find(|message| message["role"] == "assistant")
            .unwrap();
        assert_eq!(
            user["content"],
            "保留 x_client_only 和 x_structure_only 原文"
        );
        assert_eq!(tool["content"], "{\"x_client_only\":\"tool result\"}");
        assert_eq!(
            assistant["tool_calls"][0]["function"]["arguments"],
            "{\"x_client_only\":{\"city\":\"杭州\"}}"
        );
        assert!(body["tools"][0].get("x_structure_only").is_none());
        assert!(
            body["tools"][0]["function"]
                .get("x_structure_only")
                .is_none()
        );
        assert_eq!(
            body["tools"][0]["function"]["description"],
            "保留 x_structure_only 描述"
        );
        assert_eq!(
            body["tools"][0]["function"]["parameters"],
            original["tools"][0]["function"]["parameters"]
        );
        assert_eq!(
            body["tools"][0]["function"]["parameters"]["properties"]["x_client_only"]["default"],
            json!({"x_structure_only":"杭州"})
        );
        assert_eq!(
            body["tools"][0]["function"]["parameters"]["default"],
            json!({"x_client_only":{"x_structure_only":"杭州"}})
        );
        assert_eq!(
            body["tools"][0]["function"]["parameters"]["x_schema_extension"],
            json!({"x_structure_only":true})
        );
        assert!(
            !chat
                .headers
                .iter()
                .any(|(key, _)| key.eq_ignore_ascii_case("x-client-only"))
        );
        assert!(
            !chat
                .headers
                .iter()
                .any(|(_, value)| value == "client-auth-must-not-win")
        );
        assert!(
            chat.headers
                .iter()
                .any(|(key, value)| key.eq_ignore_ascii_case("x-domain") && value == domain)
        );
        let ide_version = &chat
            .headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("x-ide-version"))
            .expect("product version header")
            .1;
        let user_agent = &chat
            .headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("user-agent"))
            .expect("product user agent")
            .1;
        assert!(user_agent.starts_with(&format!("WorkBuddy/{ide_version} ")));
        let (_, embedded_identity) = user_agent.split_once(' ').unwrap();
        let (embedded_product, _) = embedded_identity.rsplit_once(" CLI/").unwrap();
        assert!(embedded_product.ends_with(&format!("/{ide_version}")));
        for forbidden in [
            "x-model-id",
            "x-product-version",
            "origin",
            "referer",
            "x-codebuddy-request",
            "accept-language",
        ] {
            assert!(
                !chat
                    .headers
                    .iter()
                    .any(|(key, _)| key.eq_ignore_ascii_case(forbidden)),
                "{forbidden} must not be forwarded"
            );
        }
        assert!(
            chat.headers
                .iter()
                .any(|(key, value)| key.eq_ignore_ascii_case("authorization")
                    && value.contains(&access_token))
        );
        assert!(
            chat.headers
                .iter()
                .any(|(key, value)| key.eq_ignore_ascii_case("x-user-id")
                    && value == "whitelist-account")
        );
    }
}

fn stream() -> String {
    let frames = [
        json!({"id":"chat-test","model":"test-model","choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":"想"},"finish_reason":null}]}),
        json!({"id":"chat-test","model":"test-model","choices":[{"index":0,"delta":{"content":"你好","function_call":{"name":"","arguments":""}},"finish_reason":null}]}),
        json!({"id":"chat-test","model":"test-model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_weather","type":"function","function":{"name":"weather","arguments":"{\"city\":"}}]},"finish_reason":null}]}),
        json!({"id":"chat-test","model":"test-model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"杭州\"}"}}]},"finish_reason":null}]}),
        json!({"id":"chat-test","model":"test-model","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
        json!({"id":"chat-test","model":"test-model","choices":[],"usage":{"prompt_tokens":11,"completion_tokens":7,"total_tokens":18}}),
    ];
    let mut body = String::from(": heartbeat\r\n\r\n");
    for frame in frames {
        body.push_str(&format!("data: data: {frame}\r\n\r\n"));
    }
    body.push_str("data: [DONE]\r\n\r\n");
    body
}

/// 使用真实 Wasm 导出和宿主资源，阻止区域串号、Token 轮换丢失和断流伪成功。
#[tokio::test]
#[ignore = "build the release wasm32-wasip2 component first"]
async fn regional_auth_streaming_and_failure_contract() {
    let artifact = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/wasm32-wasip2/release/stravia_vendor_workbuddy.wasm");
    let bytes = std::fs::read(artifact).expect("build the release component first");
    let runtime = VendorRuntime::new().unwrap();
    let plugin = runtime.load(&bytes).await.unwrap();
    assert_eq!(plugin.descriptor().vendor_id, "workbuddy");

    for (region, domain) in [("cn", "www.workbuddy.cn"), ("intl", "www.workbuddy.ai")] {
        let origin = format!("https://{domain}");
        let local = LocalUpstream::new(&[&origin]);
        let mut provider = snapshot(region, &origin);
        local.reply(Reply::json(json!({"code":0,"data":{"state":"local-state","authUrl":format!("{origin}/login?state=local-state")}})));
        let start = run(
            &runtime,
            &plugin,
            region,
            OperationInput::Auth {
                provider: provider.clone(),
                request: AuthRequest {
                    step: AuthStep::Start {
                        redirect_uri: String::new(),
                        state: "host-state".into(),
                    },
                },
            },
            local.scope(),
        )
        .await
        .unwrap();
        let OperationOutput::Auth(AuthResponse::Authorization { url, .. }) = start else {
            panic!("expected browser authorization")
        };
        assert_eq!(url, format!("{origin}/login?state=local-state"));
        {
            let requests = local.requests.lock();
            let start = requests.last().unwrap();
            let platform = if region == "cn" {
                "workbuddy"
            } else {
                "workbuddy-ai"
            };
            assert_eq!(
                start.url,
                format!("{origin}/v2/plugin/auth/state?platform={platform}")
            );
        }

        local.reply(Reply::json(json!({"code":11217})));
        let pending = run(
            &runtime,
            &plugin,
            region,
            OperationInput::Auth {
                provider: provider.clone(),
                request: AuthRequest {
                    step: AuthStep::Poll,
                },
            },
            local.scope(),
        )
        .await
        .unwrap();
        assert!(matches!(
            pending,
            OperationOutput::Auth(AuthResponse::Pending { .. })
        ));
        local.reply(Reply::json(json!({"code":0,"data":{"accessToken":token(domain,"account-a",1),"refreshToken":"refresh-one","domain":domain,"expiresAt":4102444800_i64}})));
        let login = run(
            &runtime,
            &plugin,
            region,
            OperationInput::Auth {
                provider: provider.clone(),
                request: AuthRequest {
                    step: AuthStep::Poll,
                },
            },
            local.scope(),
        )
        .await
        .unwrap();
        let OperationOutput::Auth(AuthResponse::Credentials { values, .. }) = login else {
            panic!("expected stored credentials")
        };
        provider.credentials = values;

        local.reply(Reply::json(json!({"code":0,"data":{"data":{"accessToken":token(domain,"account-a",2),"refreshToken":"refresh-two","domain":domain}}})));
        let refresh = run(
            &runtime,
            &plugin,
            region,
            OperationInput::Auth {
                provider: provider.clone(),
                request: AuthRequest {
                    step: AuthStep::Refresh,
                },
            },
            local.scope(),
        )
        .await
        .unwrap();
        let OperationOutput::Auth(AuthResponse::Credentials { values, .. }) = refresh else {
            panic!("expected refreshed credentials")
        };
        provider.credentials = values;
        assert_eq!(provider.credentials["refresh_token"], "refresh-two");
        {
            let requests = local.requests.lock();
            let refresh = requests.last().unwrap();
            assert_eq!(
                refresh.url,
                format!("{origin}/v2/plugin/auth/token/refresh")
            );
            assert!(refresh.headers.iter().any(|(key, value)| {
                key.eq_ignore_ascii_case("x-auth-refresh-source") && value == "plugin"
            }));
        }

        local.reply(Reply::json(json!({"code":0,"data":{"endpoint":"https://untrusted.invalid","models":[{"id":"test-model","name":"Test","supportsToolCall":true,"credits":"x0.10"}],"agents":[{"name":"cli","tags":["default"],"models":["test-model"]}]}})));
        let discovered = run(
            &runtime,
            &plugin,
            region,
            OperationInput::Discover {
                provider: provider.clone(),
                request: DiscoverRequest::default(),
            },
            local.scope(),
        )
        .await
        .unwrap();
        assert_no_chat_identity_headers(local.requests.lock().last().unwrap());
        let OperationOutput::Discover(discovered) = discovered else {
            panic!("expected discovered models")
        };
        assert!(
            discovered
                .models
                .iter()
                .any(|model| model.id == "test-model")
        );

        local.reply(Reply::stream(&stream()));
        let output = run(
            &runtime,
            &plugin,
            region,
            OperationInput::Infer {
                provider: provider.clone(),
                request: request(),
            },
            local.scope(),
        )
        .await
        .unwrap();
        let wire = response_wire(output);
        assert_eq!(wire["choices"][0]["message"]["content"], "你好");
        assert_eq!(wire["choices"][0]["message"]["reasoning_content"], "想");
        assert_eq!(
            wire["choices"][0]["message"]["tool_calls"]
                .as_array()
                .unwrap()
                .len(),
            1,
            "empty legacy placeholders must not create tool calls"
        );
        assert_eq!(
            wire["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
            "{\"city\":\"杭州\"}"
        );
        assert_eq!(wire["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(wire["usage"]["total_tokens"], 18);
        {
            let requests = local.requests.lock();
            let chat = requests.last().unwrap();
            assert_eq!(chat.url, format!("{origin}/v2/chat/completions"));
            assert_chat_compression(chat, true);
            let body = application_request_body(chat);
            assert_eq!(body["model"], "test-model");
            let user = body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|message| message["role"] == "user")
                .unwrap();
            assert_eq!(user["content"], "你好");
            assert_eq!(body["tools"][0]["function"]["name"], "weather");
            assert!(
                chat.headers
                    .iter()
                    .any(|(key, value)| key.eq_ignore_ascii_case("x-domain") && value == domain)
            );
        }
        assert_eq!(
            local
                .events
                .lock()
                .iter()
                .filter(|event| matches!(event, RuntimeEvent::Completed))
                .count(),
            1
        );
        local.events.lock().clear();

        let calls = local.requests.lock().len();
        let mut crossed = provider.clone();
        crossed.credentials.insert(
            "region".into(),
            json!(if region == "cn" { "intl" } else { "cn" }),
        );
        let rejected = run(
            &runtime,
            &plugin,
            region,
            OperationInput::Infer {
                provider: crossed,
                request: request(),
            },
            local.scope(),
        )
        .await;
        assert!(rejected.is_err());
        assert_eq!(
            local.requests.lock().len(),
            calls,
            "mismatched credentials must fail before HTTP"
        );
        for invalid_base in [
            "https://copilot.tencent.com",
            "https://www.codebuddy.cn",
            if region == "cn" {
                "https://www.workbuddy.ai"
            } else {
                "https://www.workbuddy.cn"
            },
        ] {
            let mut wrong = provider.clone();
            wrong.base_url = invalid_base.into();
            assert!(
                run(
                    &runtime,
                    &plugin,
                    region,
                    OperationInput::Infer {
                        provider: wrong,
                        request: request()
                    },
                    local.scope(),
                )
                .await
                .is_err()
            );
        }
        assert_eq!(local.requests.lock().len(), calls);

        local.reply(Reply {
            status: 429,
            headers: vec![("retry-after".into(), "17".into())],
            chunks: Mutex::new(VecDeque::from([
                br#"{"error":{"message":"rate limited"}}"#.to_vec()
            ])),
        });
        let limited = run(
            &runtime,
            &plugin,
            region,
            OperationInput::Infer {
                provider: provider.clone(),
                request: request(),
            },
            local.scope(),
        )
        .await
        .unwrap_err();
        assert_eq!(limited.upstream_status(), Some(429));
        assert_eq!(limited.retry_after(), Some(Duration::from_secs(17)));

        local.reply(Reply::stream("data: {\"id\":\"cut\",\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n"));
        let truncated = run(
            &runtime,
            &plugin,
            region,
            OperationInput::Infer {
                provider: provider.clone(),
                request: request(),
            },
            local.scope(),
        )
        .await;
        assert!(
            truncated.is_err(),
            "EOF without a terminal must not become success"
        );
        assert!(
            !local
                .events
                .lock()
                .iter()
                .any(|event| matches!(event, RuntimeEvent::Completed))
        );
        assert!(
            local.replies.lock().is_empty(),
            "every planned HTTP exchange must be consumed exactly once"
        );
        println!(
            "{region}: browser auth, pending poll, token rotation, model discovery, fragmented Unicode/tools/reasoning/usage, region isolation, 429 and truncated SSE passed"
        );
    }
}

fn summary_response(rows: Vec<Value>) -> Value {
    json!({"code":0,"data":{"Packages":rows}})
}

#[tokio::test]
#[ignore = "build the release wasm32-wasip2 component first"]
async fn regional_allowance_contract() {
    let artifact = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/wasm32-wasip2/release/stravia_vendor_workbuddy.wasm");
    let runtime = VendorRuntime::new().unwrap();
    let plugin = runtime
        .load(&std::fs::read(artifact).unwrap())
        .await
        .unwrap();
    for (region, domain) in [("cn", "www.workbuddy.cn"), ("intl", "www.workbuddy.ai")] {
        let origin = format!("https://{domain}");
        let local = LocalUpstream::new(&[&origin]);
        let mut provider = snapshot(region, &origin);
        provider.credentials = BTreeMap::from([
            ("region".into(), json!(region)),
            ("domain".into(), json!(domain)),
            ("uid".into(), json!("quota-account")),
            ("enterprise_id".into(), json!("quota-tenant")),
            (
                "access_token".into(),
                json!(token(domain, "quota-account", 1)),
            ),
        ]);
        let input = || OperationInput::Allowance {
            provider: provider.clone(),
            request: AllowanceRequest::default(),
        };
        let account_response = |account: Value| {
            let data = if region == "cn" {
                account
            } else {
                json!({"accounts":[
                    {"uid":"other-account","enterpriseId":"quota-tenant","email":"other@example.test"},
                    account
                ]})
            };
            Reply::json(json!({"code":0,"data":data}))
        };
        local.reply(Reply::json(summary_response(vec![
            json!({
                "CycleTotalCapacity":500, "CycleRemainCapacity":"334.16", "CycleUsedCapacity":"165.84",
                "SubscriptionPackageName":"Resource bucket, not a verified account subscription"
            }),
            json!({
                "CycleTotalCapacity":4481, "CycleRemainCapacity":4481, "CycleUsedCapacity":0
            }),
        ])));
        local.reply(account_response(json!({
            "uid":"quota-account","enterpriseId":"quota-tenant",
            "email":" quota@example.test ","nickname":"配额昵称"
        })));
        let output = run(&runtime, &plugin, region, input(), local.scope())
            .await
            .unwrap();
        let OperationOutput::Allowance(result) = output else {
            panic!("expected allowance result")
        };
        assert_eq!(
            result.account_label.as_deref(),
            Some("quota@example.test"),
            "the matching account's readable email must be preferred over its nickname"
        );
        assert_eq!(
            result.plan_label, None,
            "a billing resource package name must not be mistaken for a subscription tier"
        );
        assert_eq!(result.allowances.len(), 1);
        assert_eq!(
            result.allowances[0].remaining.as_ref().unwrap().value,
            "4815.16"
        );
        assert_eq!(result.allowances[0].used.as_ref().unwrap().value, "165.84");
        assert_eq!(result.allowances[0].limit.as_ref().unwrap().value, "4981");
        assert_eq!(result.allowances[0].used_percent.as_deref(), Some("3.33"));
        assert_eq!(
            result.allowances[0].remaining.as_ref().unwrap().unit,
            "credits"
        );
        println!(
            "{region} aggregate row: {} | used {} credits | remaining {} credits",
            result.allowances[0].label,
            result.allowances[0].used.as_ref().unwrap().value,
            result.allowances[0].remaining.as_ref().unwrap().value
        );
        {
            let requests = local.requests.lock();
            let request = &requests[0];
            assert_eq!(
                request.url,
                format!("{origin}/billing/meter/get-user-resource-summary")
            );
            assert_eq!(request.method, "POST");
            assert_no_chat_identity_headers(request);
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body, json!({}));
            assert!(
                request
                    .headers
                    .iter()
                    .any(|(key, value)| key.eq_ignore_ascii_case("x-domain") && value == domain)
            );
            assert!(
                !request
                    .headers
                    .iter()
                    .any(|(key, _)| key.eq_ignore_ascii_case("x-ide-type"))
            );
            let profile = &requests[1];
            assert_eq!(profile.method, "GET");
            assert_eq!(
                profile.url,
                format!(
                    "{origin}/v2/plugin/{}",
                    if region == "cn" {
                        "account"
                    } else {
                        "accounts"
                    }
                )
            );
            assert!(profile.body.is_empty());
            assert_no_chat_identity_headers(profile);
            assert_eq!(request_header(profile, "x-domain"), Some(domain));
            assert_eq!(
                request_header(profile, "x-enterprise-id"),
                Some("quota-tenant")
            );
        }

        for (account, expected) in [
            (
                json!({"uid":"quota-account","enterpriseId":"quota-tenant","email":" ","nickname":" 配额昵称 "}),
                Some("配额昵称"),
            ),
            (
                json!({"uid":"other-account","enterpriseId":"quota-tenant","email":"other@example.test"}),
                None,
            ),
            (
                json!({"uid":"quota-account","enterpriseId":"other-tenant","nickname":"Other tenant"}),
                None,
            ),
            (
                json!({"uid":"quota-account","enterpriseId":"quota-tenant","nickname":" ","uin":"opaque-id","phoneNumber":"123456789"}),
                None,
            ),
        ] {
            local.reply(Reply::json(summary_response(vec![json!({
                "CycleTotalCapacity":10,"CycleRemainCapacity":7,"CycleUsedCapacity":3
            })])));
            local.reply(account_response(account));
            let output = run(&runtime, &plugin, region, input(), local.scope())
                .await
                .unwrap();
            let OperationOutput::Allowance(result) = output else {
                panic!("expected allowance result")
            };
            assert_eq!(result.account_label.as_deref(), expected);
            assert_eq!(result.allowances[0].remaining.as_ref().unwrap().value, "7");
        }

        if region == "intl" {
            local.reply(Reply::json(summary_response(Vec::new())));
            local.reply(Reply::json(json!({"code":0,"data":{"accounts":[
                {"uid":"quota-account","enterpriseId":"quota-tenant","nickname":"First"},
                {"uid":"quota-account","enterpriseId":"quota-tenant","nickname":"Second"}
            ]}})));
            let output = run(&runtime, &plugin, region, input(), local.scope())
                .await
                .unwrap();
            let OperationOutput::Allowance(result) = output else {
                panic!("expected allowance result")
            };
            assert_eq!(
                result.account_label, None,
                "ambiguous profiles must not pick the first account"
            );
        }

        local.reply(Reply::json(summary_response(vec![
            json!({"CycleTotalCapacity":10,"CycleRemainCapacity":7,"CycleUsedCapacity":3}),
            json!({"CycleTotalCapacity":10,"CycleRemainCapacity":5}),
        ])));
        assert!(
            run(&runtime, &plugin, region, input(), local.scope())
                .await
                .is_err(),
            "an incomplete summary package must not publish a partial account balance"
        );

        local.reply(Reply::json(summary_response(Vec::new())));
        local.reply(Reply {
            status: 503,
            headers: vec![("content-type".into(), "application/json".into())],
            chunks: Mutex::new(VecDeque::from([b"{}".to_vec()])),
        });
        let output = run(&runtime, &plugin, region, input(), local.scope())
            .await
            .unwrap();
        let OperationOutput::Allowance(result) = output else {
            panic!("expected explicit empty balance")
        };
        assert_eq!(result.allowances[0].remaining.as_ref().unwrap().value, "0");
        assert_eq!(result.allowances[0].used.as_ref().unwrap().value, "0");
        assert_eq!(result.allowances[0].limit.as_ref().unwrap().value, "0");
        assert_eq!(
            result.account_label, None,
            "a failed optional profile must not fail the credit refresh"
        );

        local.reply(Reply::json(summary_response(Vec::new())));
        local.reply(Reply::json(
            json!({"code":123,"data":{"nickname":"do-not-display"}}),
        ));
        let output = run(&runtime, &plugin, region, input(), local.scope())
            .await
            .unwrap();
        let OperationOutput::Allowance(result) = output else {
            panic!("expected allowance without a profile")
        };
        assert_eq!(result.account_label, None);

        local.reply(Reply::json(json!({"code":123,"msg":"do-not-echo"})));
        let error = run(&runtime, &plugin, region, input(), local.scope())
            .await
            .unwrap_err();
        assert!(
            !error
                .diagnostic_message()
                .unwrap_or_default()
                .contains("do-not-echo")
        );
        local.reply(Reply::json(json!({})));
        assert!(
            run(&runtime, &plugin, region, input(), local.scope())
                .await
                .is_err()
        );

        local.reply(Reply {
            status: 429,
            headers: vec![("retry-after".into(), "17".into())],
            chunks: Mutex::new(VecDeque::from([b"{}".to_vec()])),
        });
        let limited = run(&runtime, &plugin, region, input(), local.scope())
            .await
            .unwrap_err();
        assert_eq!(limited.upstream_status(), Some(429));
        assert_eq!(limited.retry_after(), Some(Duration::from_secs(17)));
        let before = local.requests.lock().len();
        let mut wrong = provider.clone();
        wrong.credentials.insert(
            "region".into(),
            json!(if region == "cn" { "intl" } else { "cn" }),
        );
        assert!(
            run(
                &runtime,
                &plugin,
                region,
                OperationInput::Allowance {
                    provider: wrong,
                    request: AllowanceRequest::default()
                },
                local.scope()
            )
            .await
            .is_err()
        );
        assert_eq!(local.requests.lock().len(), before);
        assert!(
            local.events.lock().is_empty(),
            "quota reads must not emit inference events"
        );
        assert!(local.replies.lock().is_empty());
        println!(
            "{region}: summary credit totals, readable account selection, account/tenant isolation, optional profile failures, incomplete-package rejection and upstream failures passed"
        );
    }
}

#[tokio::test]
#[ignore = "build the release wasm32-wasip2 component first"]
async fn connection_checks_require_matching_account_evidence_without_side_effects() {
    let artifact = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/wasm32-wasip2/release/stravia_vendor_workbuddy.wasm");
    let runtime = VendorRuntime::new().unwrap();
    let plugin = runtime
        .load(&std::fs::read(artifact).unwrap())
        .await
        .unwrap();
    for (region, domain) in [("cn", "www.workbuddy.cn"), ("intl", "www.workbuddy.ai")] {
        let origin = format!("https://{domain}");
        let local = LocalUpstream::new(&[&origin]);
        let state = b"existing-auth-and-paid-window".to_vec();
        *local.state.lock() = Some(state.clone());
        let mut provider = snapshot(region, &origin);
        provider.model = None;
        provider.credentials = BTreeMap::from([
            ("region".into(), json!(region)),
            ("domain".into(), json!(domain)),
            ("uid".into(), json!("check-account")),
            ("access_token".into(), json!(token(domain, "check-account", 1))),
        ]);
        let input = |provider| OperationInput::ConnectionCheck {
            provider,
            request: ConnectionCheckRequest::default(),
        };
        let envelope = |account: Value| {
            json!({"code":0,"data":if region == "cn" {
                account
            } else {
                json!({"accounts":[account]})
            }})
        };
        // 没有邮箱或昵称也可以证明身份；检查不依赖展示字段或可用模型。
        for enterprise in [None, Some("check-tenant")] {
            let mut current = provider.clone();
            if let Some(id) = enterprise {
                current.credentials.insert("enterprise_id".into(), json!(id));
            }
            local.reply(Reply::json(envelope(json!({
                "uid":"check-account","enterpriseId":enterprise
            }))));
            let output = run(&runtime, &plugin, region, input(current), local.scope())
                .await
                .unwrap();
            let OperationOutput::ConnectionCheck(result) = output else {
                panic!("expected connection check result")
            };
            assert_eq!(result.verified, ConnectionCheckVerification::Credentials);
        }
        for payload in [
            json!({}),
            envelope(json!({})),
            envelope(json!({"uid":"other-account"})),
            envelope(json!({"uid":"check-account","enterpriseId":"other-tenant"})),
            envelope(json!({"uid":"check-account","enterpriseId":123})),
            json!({"code":0,"data":{"accounts":[]}}),
        ] {
            local.reply(Reply::json(payload));
            let error = run(
                &runtime,
                &plugin,
                region,
                input(provider.clone()),
                local.scope(),
            )
            .await
            .unwrap_err();
            assert!(matches!(
                error,
                RuntimeError::Plugin {
                    kind: ErrorKind::InvalidResponse | ErrorKind::Upstream(_),
                    ..
                }
            ));
        }
        if region == "intl" {
            local.reply(Reply::json(json!({"code":0,"data":{"accounts":[
                {"uid":"check-account"},{"uid":"check-account"}
            ]}})));
            let error = run(
                &runtime,
                &plugin,
                region,
                input(provider.clone()),
                local.scope(),
            )
            .await
            .unwrap_err();
            assert!(matches!(
                error,
                RuntimeError::Plugin { kind: ErrorKind::InvalidResponse, .. }
            ));
        }
        for (status, body) in [
            (200, b"<html>login</html>".as_slice()),
            (200, br#"{"code":1001,"msg":"do-not-echo"}"#.as_slice()),
            (401, br#"{"error":{"message":"do-not-echo"}}"#.as_slice()),
            (429, b"{}".as_slice()),
        ] {
            local.reply(Reply {
                status,
                headers: vec![("retry-after".into(), "13".into())],
                chunks: Mutex::new(VecDeque::from([body.to_vec()])),
            });
            let before = local.requests.lock().len();
            let error = run(
                &runtime,
                &plugin,
                region,
                input(provider.clone()),
                local.scope(),
            )
            .await
            .unwrap_err();
            assert_eq!(local.requests.lock().len(), before + 1);
            if status != 200 {
                assert_eq!(error.upstream_status(), Some(status));
            }
            if status == 429 {
                assert_eq!(error.retry_after(), Some(Duration::from_secs(13)));
            }
            assert!(!error.diagnostic_message().unwrap_or_default().contains("do-not-echo"));
        }
        let before = local.requests.lock().len();
        provider.credentials.insert(
            "region".into(),
            json!(if region == "cn" { "intl" } else { "cn" }),
        );
        let error = run(&runtime, &plugin, region, input(provider), local.scope())
            .await
            .unwrap_err();
        assert!(matches!(error, RuntimeError::Plugin { kind: ErrorKind::Auth, .. }));
        assert_eq!(local.requests.lock().len(), before);
        for request in local.requests.lock().iter() {
            assert_eq!(request.method, "GET");
            assert_eq!(
                request.url,
                format!("{origin}/v2/plugin/{}", if region == "cn" { "account" } else { "accounts" })
            );
            assert_no_chat_identity_headers(request);
        }
        assert_eq!(*local.state.lock(), Some(state));
        assert!(local.events.lock().is_empty());
        assert!(local.replies.lock().is_empty());
        println!("{region}: credential evidence, identity isolation, response failures and read-only connection checks passed");
    }
}

#[tokio::test]
#[ignore = "build the release wasm32-wasip2 component first"]
async fn default_agent_models_merge_free_and_paid_lines() {
    let artifact = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/wasm32-wasip2/release/stravia_vendor_workbuddy.wasm");
    let runtime = VendorRuntime::new().unwrap();
    let plugin = runtime
        .load(&std::fs::read(artifact).unwrap())
        .await
        .unwrap();
    let local = LocalUpstream::new(&["https://www.workbuddy.cn"]);
    let mut provider = snapshot("cn", "https://www.workbuddy.cn");
    provider.credentials = BTreeMap::from([
        ("region".into(), json!("cn")),
        ("domain".into(), json!("www.workbuddy.cn")),
        ("uid".into(), json!("account-a")),
        ("enterprise_id".into(), json!("discovery-tenant")),
        (
            "access_token".into(),
            json!(token("www.workbuddy.cn", "account-a", 1)),
        ),
    ]);
    local.reply(Reply::json(json!({"code":0,"data":{
        "deploymentType":"configured-product",
        "productFeatures":{"RequestBodyGzip":false,"DisableYuanbaoChannel":true},
        "models":[
            {"id":"old-model","name":"Old model","credits":"x0.79"},
            {"id":"hy4-preview","name":"Hy4 preview","credits":"x0.29"},
            {"id":"hy4-preview-f","name":"Hy4 free deployment","credits":"x0.00"},
            {"id":"hy3-x","name":"Hy3","credits":"x0.05"},
            {"id":"hy3","name":"Hy3","credits":"x0.00"},
            {"id":"same-a","name":"Same name","credits":"x0.10"},
            {"id":"same-b","name":"Same name","credits":"x0.20"}
        ],
        "agents":[
            {"name":"cli","models":["old-model"]},
            {"name":"desktop","tags":["default"],"models":["hy4-preview-f","hy3","hy3-x","same-b","same-a"]}
        ],
        "productFeaturesConfig":{"ModelRateLimitCap":{"lines":[
            {"freeId":"hy3","paidId":"hy3-x","displayName":"Hy3","allowPaidSwitch":true},
            {"freeId":"hy4-preview-f","paidId":"hy4-preview","displayName":"Hy4 preview","allowPaidSwitch":true}
        ]}}
    }})));
    let output = run(
        &runtime,
        &plugin,
        "cn",
        OperationInput::Discover {
            provider: provider.clone(),
            request: DiscoverRequest::default(),
        },
        local.scope(),
    )
    .await
    .unwrap();
    let OperationOutput::Discover(discovered) = output else {
        panic!("expected discovery");
    };
    assert_eq!(
        discovered
            .models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>(),
        ["hy4-preview-f", "hy3", "same-b", "same-a"],
        "use the default agent's order, hide paired paid IDs, and retain unrelated same-name models"
    );
    assert_eq!(
        discovered.models[0].display_name, "Hy4 preview",
        "the configured paired display name overrides the deployment name"
    );
    for agents in [
        json!([]),
        json!([{"name":"empty-default","tags":["default"],"models":[]},{"name":"cli","models":["old-model"]}]),
    ] {
        local.reply(Reply::json(json!({"code":0,"data":{
            "models":[{"id":"old-model","name":"Old model","credits":"x1"}], "agents":agents
        }})));
        let result = run(
            &runtime,
            &plugin,
            "cn",
            OperationInput::Discover {
                provider: provider.clone(),
                request: DiscoverRequest::default(),
            },
            local.scope(),
        )
        .await;
        assert!(
            result.is_err(),
            "a missing or empty default roster must not expose a different agent or the full catalog"
        );
    }
    assert!(local.events.lock().is_empty());
    assert!(local.replies.lock().is_empty());
    let mut selected = discovered.models.into_iter().nth(1).unwrap();
    assert_eq!(
        selected
            .metadata
            .get("workbuddy_disable_model_optimization"),
        Some(&json!(true)),
        "the official privacy switch must reach the selected model"
    );
    selected.metadata.get_mut("workbuddy_paid_line").unwrap()["paidMetadata"]["workbuddy_disable_model_optimization"] =
        json!(false);
    assert_eq!(
        selected.metadata.get("workbuddy_request_body_gzip"),
        Some(&json!(false)),
        "the official RequestBodyGzip switch must reach the selected model"
    );
    provider.model = Some(selected.id);
    provider.model_metadata = Some(stravia_vendor_sdk::ModelMetadata {
        extensions: selected.metadata,
        ..Default::default()
    });
    provider
        .options
        .insert("auto_paid_on_rate_limit".into(), json!(true));
    local.reply(Reply::json(json!({"code":6004})));
    local.reply(Reply::stream(&completion_stream(
        "hy3-x",
        "configured transport",
    )));
    let output = run(
        &runtime,
        &plugin,
        "cn",
        OperationInput::Infer {
            provider,
            request: request(),
        },
        local.scope(),
    )
    .await
    .unwrap();
    assert_eq!(
        response_wire(output)["choices"][0]["message"]["content"],
        "configured transport"
    );
    assert_eq!(requested_models(&local), ["hy3", "hy3-x"]);
    for wire_request in local.requests.lock().iter() {
        if wire_request.url.ends_with("/v2/chat/completions") {
            assert_chat_compression(wire_request, false);
            assert_eq!(request_header(wire_request, "x-private-data"), Some("true"));
            assert_eq!(
                request_header(wire_request, "x-product"),
                Some("configured-product")
            );
            assert_eq!(
                application_request_body(wire_request)["messages"][1]["content"],
                "你好"
            );
        } else {
            assert_no_chat_identity_headers(wire_request);
        }
    }
    println!(
        "default agent: roster and paid lines preserved; configured transport honored across a paid retry"
    );
}

#[tokio::test]
#[ignore = "build the release wasm32-wasip2 component first"]
async fn regional_chat_identity_isolation_and_client_header_injection_contract() {
    let artifact = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/wasm32-wasip2/release/stravia_vendor_workbuddy.wasm");
    let runtime = VendorRuntime::new().unwrap();
    let plugin = runtime
        .load(&std::fs::read(artifact).unwrap())
        .await
        .unwrap();
    for (region, domain) in [("cn", "www.workbuddy.cn"), ("intl", "www.workbuddy.ai")] {
        let origin = format!("https://{domain}");
        let local = LocalUpstream::new(&[&origin]);
        let mut provider = snapshot(region, &origin);
        let tenant = format!("tenant-{region}");
        provider.credentials = BTreeMap::from([
            ("region".into(), json!(region)),
            ("domain".into(), json!(domain)),
            ("uid".into(), json!("identity-account")),
            ("enterprise_id".into(), json!(tenant)),
            (
                "access_token".into(),
                json!(token(domain, "identity-account", 1)),
            ),
        ]);
        let source_only_headers = [
            "x-auth-method",
            "x-id-source",
            "x-device-token",
            "x-device-id-list",
            "x-parent-conversation-id",
            "x-compaction",
            "x-context-compacted",
            "x-stainless-client-only",
            "x-stainless-helper-method",
            "x-stainless-poll-helper",
        ];
        let sdk_headers = [
            "x-stainless-lang",
            "x-stainless-package-version",
            "x-stainless-os",
            "x-stainless-arch",
            "x-stainless-runtime",
            "x-stainless-runtime-version",
            "x-stainless-retry-count",
            "x-stainless-timeout",
        ];
        provider.client_headers = source_only_headers
            .iter()
            .chain(sdk_headers.iter())
            .map(|name| ((*name).into(), "client-injected".into()))
            .chain([
                ("X-Userinfo".into(), "client-injected-userinfo".into()),
                ("X-Private-Data".into(), "true".into()),
                ("X-Agent-Type".into(), "team".into()),
                ("X-Agent-Intent".into(), "client-injected-intent".into()),
                ("X-Agent-Purpose".into(), "client-injected-purpose".into()),
                ("X-Enterprise-Id".into(), "client-injected-tenant".into()),
                ("X-Tenant-Id".into(), "client-injected-tenant".into()),
            ])
            .collect();
        for enterprise in [true, false] {
            if !enterprise {
                provider.credentials.remove("enterprise_id");
            }
            let answer = if enterprise {
                "企业回答"
            } else {
                "个人回答"
            };
            local.reply(Reply::stream(&completion_stream("test-model", answer)));
            let output = run(
                &runtime,
                &plugin,
                region,
                OperationInput::Infer {
                    provider: provider.clone(),
                    request: request(),
                },
                local.scope(),
            )
            .await
            .unwrap();
            assert_eq!(
                response_wire(output)["choices"][0]["message"]["content"],
                answer
            );
            let requests = local.requests.lock();
            let wire = requests.last().unwrap();
            assert_eq!(request_header(wire, "x-private-data"), Some("false"));
            assert_eq!(request_header(wire, "x-stainless-os"), Some("Windows"));
            assert_eq!(request_header(wire, "x-stainless-arch"), Some("x64"));
            for name in sdk_headers {
                assert_ne!(
                    request_header(wire, name),
                    Some("client-injected"),
                    "client input cannot override the fixed SDK profile"
                );
                assert_eq!(
                    wire.headers
                        .iter()
                        .filter(|(key, _)| key.eq_ignore_ascii_case(name))
                        .count(),
                    1,
                    "a spoofed SDK header must not coexist with the fixed profile"
                );
            }
            for (name, spoofed) in [
                ("x-agent-type", "team"),
                ("x-agent-intent", "client-injected-intent"),
                ("x-agent-purpose", "client-injected-purpose"),
            ] {
                let actual = request_header(wire, name).expect("ordinary chat profile is present");
                assert_ne!(
                    actual, spoofed,
                    "client headers cannot select an agent profile"
                );
            }
            for name in source_only_headers {
                assert!(
                    request_header(wire, name).is_none(),
                    "{name} cannot be created without an authentic source"
                );
            }
            if enterprise {
                let encoded = request_header(wire, "x-userinfo").unwrap();
                let decoded: Value =
                    serde_json::from_slice(&STANDARD.decode(encoded).unwrap()).unwrap();
                assert_eq!(
                    decoded,
                    json!({"uin":"identity-account","owner_uin":tenant})
                );
                assert_eq!(
                    request_header(wire, "x-enterprise-id"),
                    Some(tenant.as_str())
                );
                assert_eq!(request_header(wire, "x-tenant-id"), Some(tenant.as_str()));
                assert_eq!(
                    wire.headers
                        .iter()
                        .filter(|(name, _)| name.eq_ignore_ascii_case("x-userinfo"))
                        .count(),
                    1,
                    "a spoofed identity must not survive alongside the real identity"
                );
            } else {
                for name in ["x-userinfo", "x-enterprise-id", "x-tenant-id"] {
                    assert!(
                        request_header(wire, name).is_none(),
                        "personal requests must not retain or accept enterprise identity"
                    );
                }
            }
        }
        let before = local.requests.lock().len();
        provider
            .credentials
            .insert("enterprise_id".into(), json!("tenant\r\nx-injected: yes"));
        assert!(
            run(
                &runtime,
                &plugin,
                region,
                OperationInput::Infer {
                    provider,
                    request: request(),
                },
                local.scope(),
            )
            .await
            .is_err(),
            "an invalid enterprise header value must be rejected before HTTP"
        );
        assert_eq!(local.requests.lock().len(), before);
        assert!(local.replies.lock().is_empty());
    }
}

fn paid_provider() -> ProviderSnapshot {
    let mut provider = snapshot("cn", "https://www.workbuddy.cn");
    provider.model = Some("hy3".into());
    provider.credentials = BTreeMap::from([
        ("region".into(), json!("cn")),
        ("domain".into(), json!("www.workbuddy.cn")),
        ("uid".into(), json!("account-a")),
        (
            "access_token".into(),
            json!(token("www.workbuddy.cn", "account-a", 1)),
        ),
    ]);
    provider.model_metadata = Some(stravia_vendor_sdk::ModelMetadata {
        id: Some("hy3".into()),
        extensions: BTreeMap::from([(
            "workbuddy_paid_line".into(),
            json!({
                "freeId":"hy3", "paidId":"hy3-x", "allowPaidSwitch":true,
                "paidMetadata":{"reasoning_default_effort":"high"}
            }),
        )]),
        ..Default::default()
    });
    provider
}

fn completion_stream(model: &str, text: &str) -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"id":"paid-test","model":model,"choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]}),
        json!({"id":"paid-test","model":model,"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})
    )
}

fn requested_models(local: &LocalUpstream) -> Vec<String> {
    local
        .requests
        .lock()
        .iter()
        .filter(|request| request.method == "POST" && request.url.ends_with("/v2/chat/completions"))
        .map(|request| {
            let body = application_request_body(request);
            let model = body["model"].as_str().unwrap();
            model.to_owned()
        })
        .collect()
}

#[tokio::test]
#[ignore = "build the release wasm32-wasip2 component first"]
async fn request_context_survives_retry_and_separates_model_calls() {
    let artifact = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/wasm32-wasip2/release/stravia_vendor_workbuddy.wasm");
    let component = std::fs::read(artifact).unwrap();
    let runtime = VendorRuntime::new().unwrap();
    let plugin = runtime.load(&component).await.unwrap();
    let local = LocalUpstream::new(&["https://www.workbuddy.cn"]);
    let mut provider = paid_provider();
    provider
        .options
        .insert("auto_paid_on_rate_limit".into(), json!(true));
    provider.operation_metadata = BTreeMap::from([
        ("root_request_id".into(), json!("user-run-a")),
        (
            "traceparent".into(),
            json!("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
        ),
        ("tracestate".into(), json!("vendor=entry")),
        (
            "session_affinity".into(),
            json!("0123456789abcdef".repeat(4)),
        ),
    ]);
    provider.client_headers = vec![
        ("x-root-request-id".into(), "client-root".into()),
        (
            "traceparent".into(),
            "00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbbbbbbbbbb-00".into(),
        ),
        ("x-conversation-id".into(), "client-session".into()),
        ("x-conversation-request-id".into(), "client-request".into()),
        ("x-conversation-message-id".into(), "client-message".into()),
        ("acp-connection-id".into(), "client-fake-connection".into()),
    ];
    local.reply(Reply::json(json!({"code":6004})));
    local.reply(Reply::stream(&completion_stream("hy3-x", "retry")));
    let retry = run(
        &runtime,
        &plugin,
        "cn",
        OperationInput::Infer {
            provider: provider.clone(),
            request: request(),
        },
        local.scope(),
    )
    .await
    .unwrap();
    assert_eq!(
        response_wire(retry)["choices"][0]["message"]["content"],
        "retry"
    );

    local.reply(Reply::stream(&completion_stream("hy3", "child")));
    let child = run(
        &runtime,
        &plugin,
        "cn",
        OperationInput::Infer {
            provider: provider.clone(),
            request: request(),
        },
        local.scope(),
    )
    .await
    .unwrap();
    assert_eq!(
        response_wire(child)["choices"][0]["message"]["content"],
        "child"
    );

    provider
        .operation_metadata
        .insert("root_request_id".into(), json!("user-run-b"));
    provider.operation_metadata.insert(
        "traceparent".into(),
        json!("00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbbbbbbbbbb-00"),
    );
    provider.operation_metadata.remove("tracestate");
    local.reply(Reply::stream(&completion_stream("hy3", "next turn")));
    let next = run(
        &runtime,
        &plugin,
        "cn",
        OperationInput::Infer {
            provider: provider.clone(),
            request: request(),
        },
        local.scope(),
    )
    .await
    .unwrap();
    assert_eq!(
        response_wire(next)["choices"][0]["message"]["content"],
        "next turn"
    );

    let reloaded_runtime = VendorRuntime::new().unwrap();
    let reloaded_plugin = reloaded_runtime.load(&component).await.unwrap();
    local.reply(Reply::stream(&completion_stream("hy3", "reloaded")));
    run(
        &reloaded_runtime,
        &reloaded_plugin,
        "cn",
        OperationInput::Infer {
            provider: provider.clone(),
            request: request(),
        },
        local.scope(),
    )
    .await
    .unwrap();

    provider
        .operation_metadata
        .insert("session_affinity".into(), json!("another-host-session"));
    local.reply(Reply::stream(&completion_stream("hy3", "new session")));
    run(
        &reloaded_runtime,
        &reloaded_plugin,
        "cn",
        OperationInput::Infer {
            provider,
            request: request(),
        },
        local.scope(),
    )
    .await
    .unwrap();

    assert_eq!(
        requested_models(&local),
        ["hy3", "hy3-x", "hy3", "hy3", "hy3", "hy3"]
    );
    let requests = local.requests.lock();
    let first = &requests[0];
    let retry = &requests[1];
    let child = &requests[2];
    let next = &requests[3];
    let reloaded = &requests[4];
    let new_session = &requests[5];
    assert_eq!(
        application_request_body(first)["messages"],
        application_request_body(retry)["messages"]
    );
    for wire in requests.iter() {
        assert_chat_compression(wire, true);
        assert_eq!(request_header(wire, "x-conversation-id").unwrap().len(), 36);
        for name in [
            "x-root-request-id",
            "x-conversation-request-id",
            "x-conversation-message-id",
            "x-request-id",
        ] {
            let value = request_header(wire, name).unwrap();
            assert_eq!(value.len(), 32, "{name} must omit UUID separators");
            assert!(
                value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                "{name} must use lowercase hexadecimal"
            );
            let id = uuid::Uuid::parse_str(value).unwrap();
            assert_eq!(id.get_version(), Some(uuid::Version::SortRand));
            assert_eq!(id.get_variant(), uuid::Variant::RFC4122);
        }
        assert_eq!(
            request_header(wire, "x-stainless-retry-count"),
            Some("0"),
            "paid routing and separate model calls are not SDK transport retries"
        );
        assert_eq!(request_header(wire, "x-product"), Some("SaaS"));
        assert!(
            request_header(wire, "acp-connection-id").is_none(),
            "client input cannot invent an ACP connection"
        );
        assert_eq!(
            request_header(wire, "x-request-id"),
            request_header(wire, "x-conversation-message-id")
        );
    }
    for wire in &requests[..3] {
        assert_eq!(
            request_header(wire, "x-root-request-id"),
            Some("c845b199d22c77b88bf012ecdc201766")
        );
        assert_eq!(
            request_header(wire, "x-b3-traceid"),
            Some("4bf92f3577b34da6a3ce929d0e0e4736")
        );
        assert_eq!(
            request_header(wire, "x-b3-parentspanid"),
            Some("00f067aa0ba902b7")
        );
        assert_eq!(request_header(wire, "x-b3-sampled"), Some("1"));
        assert_eq!(request_header(wire, "tracestate"), Some("vendor=entry"));
        let http_span = request_header(wire, "traceparent")
            .unwrap()
            .split('-')
            .nth(2)
            .unwrap();
        assert_ne!(
            Some(http_span),
            request_header(wire, "x-b3-spanid"),
            "HTTP and model spans represent different operations"
        );
        assert_eq!(
            request_header(wire, "b3").unwrap(),
            format!(
                "4bf92f3577b34da6a3ce929d0e0e4736-{}-1-00f067aa0ba902b7",
                request_header(wire, "x-b3-spanid").unwrap()
            ),
        );
    }
    for name in [
        "x-root-request-id",
        "x-conversation-request-id",
        "baggage",
        "b3",
        "x-b3-spanid",
    ] {
        assert_eq!(
            request_header(first, name),
            request_header(retry, name),
            "{name} must survive an internal retry"
        );
    }
    assert_ne!(
        request_header(first, "x-request-id"),
        request_header(retry, "x-request-id")
    );
    assert_ne!(
        request_header(first, "traceparent"),
        request_header(retry, "traceparent")
    );
    assert_ne!(
        request_header(first, "x-b3-spanid"),
        request_header(child, "x-b3-spanid")
    );
    assert_eq!(
        request_header(first, "x-conversation-id"),
        request_header(next, "x-conversation-id")
    );
    assert_eq!(
        request_header(first, "x-conversation-id"),
        Some("16e7ce41-5946-7f64-a10a-02d18223460e")
    );
    assert_eq!(
        request_header(next, "x-root-request-id"),
        Some("29f6aa535a2a79c99ec09e843f64b19f")
    );
    assert_eq!(
        request_header(next, "x-b3-traceid"),
        Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
    );
    assert_eq!(request_header(next, "x-b3-sampled"), Some("0"));
    assert!(request_header(next, "tracestate").is_none());
    for name in ["x-root-request-id", "x-conversation-id"] {
        assert_eq!(
            request_header(next, name),
            request_header(reloaded, name),
            "{name} must be independent of the plugin instance"
        );
    }
    assert_eq!(
        request_header(reloaded, "x-root-request-id"),
        request_header(new_session, "x-root-request-id"),
        "changing the session source must not change the run identity"
    );
    assert_ne!(
        request_header(reloaded, "x-conversation-id"),
        request_header(new_session, "x-conversation-id"),
        "distinct host sessions must not share the upstream cache identity"
    );
    assert_ne!(
        request_header(next, "x-conversation-request-id"),
        request_header(reloaded, "x-conversation-request-id"),
        "a new model operation must retain its own request identity"
    );
    assert!(
        request_header(first, "baggage")
            .unwrap()
            .ends_with(&format!(
                "codebuddy.conversation_request_id={}",
                request_header(first, "x-conversation-request-id").unwrap()
            ))
    );
}

#[tokio::test]
#[ignore = "build the release wasm32-wasip2 component first"]
async fn automatic_paid_line_switch_and_window_contract() {
    let artifact = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/wasm32-wasip2/release/stravia_vendor_workbuddy.wasm");
    let runtime = VendorRuntime::new().unwrap();
    let plugin = runtime
        .load(&std::fs::read(artifact).unwrap())
        .await
        .unwrap();
    for shape in [
        "sse-future-window",
        "sse-eof-no-reset",
        "json-no-reset",
        "json-nested-future",
        "http-future-window",
    ] {
        let local = LocalUpstream::new(&["https://www.workbuddy.cn", "https://www.workbuddy.ai"]);
        let mut provider = paid_provider();
        provider
            .options
            .insert("auto_paid_on_rate_limit".into(), json!(true));
        let cap = match shape {
            "sse-future-window" => {
                json!({"code":6004,"resetAt":4102444800000_i64,"message":"do-not-echo"})
            }
            "http-future-window" => {
                json!({"code":"6004","resetAt":4102444800000_i64,"message":"do-not-echo"})
            }
            "json-nested-future" => {
                json!({"error":{"code":"6004","resetAt":"2099-01-01T00:00:00Z","message":"do-not-echo"}})
            }
            _ => json!({"code":6004,"message":"do-not-echo"}),
        };
        match shape {
            "sse-future-window" => local.reply(Reply::stream(&format!("data: data: {cap}\n\n"))),
            "sse-eof-no-reset" => local.reply(Reply::stream(&format!("data: {cap}"))),
            "http-future-window" => local.reply(Reply {
                status: 429,
                headers: vec![("content-type".into(), "application/json".into())],
                chunks: Mutex::new(VecDeque::from([serde_json::to_vec(&cap).unwrap()])),
            }),
            _ => local.reply(Reply::json(cap)),
        }
        local.reply(Reply::stream(&completion_stream("hy3-x", "paid answer")));
        let output = run(
            &runtime,
            &plugin,
            "cn",
            OperationInput::Infer {
                provider: provider.clone(),
                request: request_with_unknown(),
            },
            local.scope(),
        )
        .await
        .unwrap();
        assert_eq!(
            response_wire(output)["choices"][0]["message"]["content"],
            "paid answer"
        );
        assert_eq!(requested_models(&local), ["hy3", "hy3-x"]);
        for attempt in local.requests.lock().iter() {
            assert_chat_compression(attempt, true);
            let body = application_request_body(attempt);
            assert!(
                body.get("x_client_only").is_none(),
                "both free and paid attempts must filter unknown fields"
            );
        }
        assert_eq!(
            local
                .events
                .lock()
                .iter()
                .filter(|event| matches!(event, RuntimeEvent::Completed))
                .count(),
            1
        );
        assert!(
            !local
                .events
                .lock()
                .iter()
                .any(|event| matches!(event, RuntimeEvent::Failed { .. }))
        );
        local.events.lock().clear();

        let next_model = if shape.contains("future") {
            "hy3-x"
        } else {
            "hy3"
        };
        local.reply(Reply::stream(&completion_stream(next_model, "next answer")));
        let output = run(
            &runtime,
            &plugin,
            "cn",
            OperationInput::Infer {
                provider: provider.clone(),
                request: request(),
            },
            local.scope(),
        )
        .await
        .unwrap();
        assert_eq!(
            response_wire(output)["choices"][0]["message"]["content"],
            "next answer"
        );
        assert_eq!(requested_models(&local).last().unwrap(), next_model);

        if shape == "sse-future-window" {
            provider
                .options
                .insert("auto_paid_on_rate_limit".into(), json!(false));
            local.reply(Reply::stream(&completion_stream(
                "hy3",
                "free after opt-out",
            )));
            let output = run(
                &runtime,
                &plugin,
                "cn",
                OperationInput::Infer {
                    provider: provider.clone(),
                    request: request(),
                },
                local.scope(),
            )
            .await
            .unwrap();
            assert_eq!(
                response_wire(output)["choices"][0]["message"]["content"],
                "free after opt-out"
            );
            assert_eq!(requested_models(&local).last().unwrap(), "hy3");

            provider
                .options
                .insert("auto_paid_on_rate_limit".into(), json!(true));
            let mut other_account = provider.clone();
            other_account
                .credentials
                .insert("uid".into(), json!("account-b"));
            other_account.credentials.insert(
                "access_token".into(),
                json!(token("www.workbuddy.cn", "account-b", 2)),
            );
            local.reply(Reply::stream(&completion_stream(
                "hy3",
                "other account free",
            )));
            let output = run(
                &runtime,
                &plugin,
                "cn",
                OperationInput::Infer {
                    provider: other_account,
                    request: request(),
                },
                local.scope(),
            )
            .await
            .unwrap();
            assert_eq!(
                response_wire(output)["choices"][0]["message"]["content"],
                "other account free"
            );
            assert_eq!(requested_models(&local).last().unwrap(), "hy3");

            let mut other_tenant = provider.clone();
            other_tenant
                .credentials
                .insert("enterprise_id".into(), json!("tenant-b"));
            local.reply(Reply::stream(&completion_stream(
                "hy3",
                "other tenant free",
            )));
            let output = run(
                &runtime,
                &plugin,
                "cn",
                OperationInput::Infer {
                    provider: other_tenant,
                    request: request(),
                },
                local.scope(),
            )
            .await
            .unwrap();
            assert_eq!(
                response_wire(output)["choices"][0]["message"]["content"],
                "other tenant free"
            );
            assert_eq!(requested_models(&local).last().unwrap(), "hy3");

            let mut other_region = provider.clone();
            other_region.channel = "intl".into();
            other_region.base_url = "https://www.workbuddy.ai".into();
            other_region
                .credentials
                .insert("region".into(), json!("intl"));
            other_region
                .credentials
                .insert("domain".into(), json!("www.workbuddy.ai"));
            other_region.credentials.insert(
                "access_token".into(),
                json!(token("www.workbuddy.ai", "account-a", 3)),
            );
            local.reply(Reply::stream(&completion_stream(
                "hy3",
                "other region free",
            )));
            let output = run(
                &runtime,
                &plugin,
                "intl",
                OperationInput::Infer {
                    provider: other_region,
                    request: request(),
                },
                local.scope(),
            )
            .await
            .unwrap();
            assert_eq!(
                response_wire(output)["choices"][0]["message"]["content"],
                "other region free"
            );
            assert_eq!(requested_models(&local).last().unwrap(), "hy3");

            // 用已到期的持久化时间模拟窗口结束，避免依赖等待和调度时序。
            {
                let mut stored = local.state.lock();
                let mut root: Value = serde_json::from_slice(stored.as_deref().unwrap()).unwrap();
                for reset in root["workbuddy_paid_windows"]
                    .as_object_mut()
                    .unwrap()
                    .values_mut()
                {
                    *reset = json!(1);
                }
                *stored = Some(serde_json::to_vec(&root).unwrap());
            }
            local.reply(Reply::stream(&completion_stream(
                "hy3",
                "free after window expiry",
            )));
            let output = run(
                &runtime,
                &plugin,
                "cn",
                OperationInput::Infer {
                    provider,
                    request: request(),
                },
                local.scope(),
            )
            .await
            .unwrap();
            assert_eq!(
                response_wire(output)["choices"][0]["message"]["content"],
                "free after window expiry"
            );
            assert_eq!(requested_models(&local).last().unwrap(), "hy3");
        }
        assert!(local.replies.lock().is_empty());
        println!(
            "{shape}: explicit 6004 switched once before output; reset window, opt-out and account isolation respected"
        );
    }
}

#[tokio::test]
#[ignore = "build the release wasm32-wasip2 component first"]
async fn automatic_paid_line_rejects_unsafe_retries() {
    let artifact = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/wasm32-wasip2/release/stravia_vendor_workbuddy.wasm");
    let runtime = VendorRuntime::new().unwrap();
    let plugin = runtime
        .load(&std::fs::read(artifact).unwrap())
        .await
        .unwrap();
    for scenario in [
        "default-off",
        "ordinary-429",
        "expired-window",
        "other-business-code",
        "partial-output",
        "switch-disallowed",
        "paid-fails",
        "invalid-option",
    ] {
        let local = LocalUpstream::new(&["https://www.workbuddy.cn"]);
        let mut provider = paid_provider();
        if scenario != "default-off" {
            provider
                .options
                .insert("auto_paid_on_rate_limit".into(), json!(true));
        }
        if scenario == "switch-disallowed" {
            provider
                .model_metadata
                .as_mut()
                .unwrap()
                .extensions
                .get_mut("workbuddy_paid_line")
                .unwrap()["allowPaidSwitch"] = json!(false);
        }
        match scenario {
            "ordinary-429" => local.reply(Reply {
                status:429,headers:vec![],chunks:Mutex::new(VecDeque::from([br#"{"error":{"message":"limited"}}"#.to_vec()]))
            }),
            "expired-window" => local.reply(Reply {
                status:429,headers:vec![],chunks:Mutex::new(VecDeque::from([br#"{"code":6004,"resetAt":1}"#.to_vec()]))
            }),
            "other-business-code" => local.reply(Reply::json(json!({"code":6005}))),
            "partial-output" => local.reply(Reply::stream(&format!(
                "data: {}\n\ndata: {}\n\n",
                json!({"id":"partial","model":"hy3","choices":[{"index":0,"delta":{"content":"already visible"},"finish_reason":null}]}),
                json!({"code":6004,"resetAt":4102444800000_i64})
            ))),
            "invalid-option" => { provider.options.insert("auto_paid_on_rate_limit".into(),json!("true")); },
            _ => local.reply(Reply::json(json!({"code":6004}))),
        }
        if scenario == "paid-fails" {
            local.reply(Reply::json(json!({"code":6004})));
        }
        let result = run(
            &runtime,
            &plugin,
            "cn",
            OperationInput::Infer {
                provider,
                request: request(),
            },
            local.scope(),
        )
        .await;
        assert!(
            result.is_err(),
            "{scenario} must not turn an upstream failure into success"
        );
        let expected = match scenario {
            "invalid-option" => Vec::new(),
            "paid-fails" => vec!["hy3".to_owned(), "hy3-x".to_owned()],
            _ => vec!["hy3".to_owned()],
        };
        assert_eq!(
            requested_models(&local),
            expected,
            "{scenario}: no unauthorized or duplicate paid attempt"
        );
        assert!(
            !local
                .events
                .lock()
                .iter()
                .any(|event| matches!(event, RuntimeEvent::Completed))
        );
        assert!(local.replies.lock().is_empty());
        println!("{scenario}: no unsafe retry or false successful completion");
    }
}

#[tokio::test]
#[ignore = "build the release wasm32-wasip2 component first"]
async fn configured_model_resolves_paid_line_after_cap() {
    let artifact = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/wasm32-wasip2/release/stravia_vendor_workbuddy.wasm");
    let runtime = VendorRuntime::new().unwrap();
    let plugin = runtime
        .load(&std::fs::read(artifact).unwrap())
        .await
        .unwrap();
    let local = LocalUpstream::new(&["https://www.workbuddy.cn"]);
    let mut provider = paid_provider();
    provider.model_metadata = None;
    provider.options.insert("model_ids".into(), json!("hy3"));
    provider
        .options
        .insert("auto_paid_on_rate_limit".into(), json!(true));
    local.reply(Reply::json(
        json!({"code":6004,"resetAt":4102444800000_i64}),
    ));
    let paired_config = json!({"code":0,"data":{
        "models":[{"id":"hy3","name":"Hy3"},{"id":"hy3-x","name":"Hy3"}],
        "agents":[{"name":"cli","tags":["default"],"models":["hy3","hy3-x"]}],
        "productFeaturesConfig":{"ModelRateLimitCap":{"lines":[
            {"freeId":"hy3","paidId":"hy3-x","allowPaidSwitch":true}
        ]}}
    }});
    local.reply(Reply::json(paired_config.clone()));
    local.reply(Reply::stream(&completion_stream(
        "hy3-x",
        "paid custom model",
    )));
    let output = run(
        &runtime,
        &plugin,
        "cn",
        OperationInput::Infer {
            provider: provider.clone(),
            request: request(),
        },
        local.scope(),
    )
    .await
    .unwrap();
    assert_eq!(
        response_wire(output)["choices"][0]["message"]["content"],
        "paid custom model"
    );
    assert_eq!(requested_models(&local), ["hy3", "hy3-x"]);
    local.reply(Reply::json(paired_config));
    local.reply(Reply::stream(&completion_stream(
        "hy3-x",
        "custom model inside window",
    )));
    let output = run(
        &runtime,
        &plugin,
        "cn",
        OperationInput::Infer {
            provider,
            request: request(),
        },
        local.scope(),
    )
    .await
    .unwrap();
    assert_eq!(
        response_wire(output)["choices"][0]["message"]["content"],
        "custom model inside window"
    );
    assert_eq!(requested_models(&local), ["hy3", "hy3-x", "hy3-x"]);
    assert!(local.replies.lock().is_empty());
    println!(
        "explicit model ID without metadata: resolves the authoritative pair after 6004 and revalidates it for direct paid use inside the window"
    );
}

#[tokio::test]
#[ignore = "build the release wasm32-wasip2 component first"]
async fn paid_window_survives_login_and_revoke_clears_it() {
    let artifact = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/wasm32-wasip2/release/stravia_vendor_workbuddy.wasm");
    let runtime = VendorRuntime::new().unwrap();
    let plugin = runtime
        .load(&std::fs::read(artifact).unwrap())
        .await
        .unwrap();
    let local = LocalUpstream::new(&["https://www.workbuddy.cn"]);
    let mut provider = paid_provider();
    provider
        .options
        .insert("auto_paid_on_rate_limit".into(), json!(true));
    local.reply(Reply::json(
        json!({"code":6004,"message":"usage exceeds frequency limit until 2099-01-01 08:00:00"}),
    ));
    local.reply(Reply::stream(&completion_stream(
        "hy3-x",
        "paid before login",
    )));
    let output = run(
        &runtime,
        &plugin,
        "cn",
        OperationInput::Infer {
            provider: provider.clone(),
            request: request(),
        },
        local.scope(),
    )
    .await
    .unwrap();
    assert_eq!(
        response_wire(output)["choices"][0]["message"]["content"],
        "paid before login"
    );

    local.reply(Reply::json(json!({"code":0,"data":{"state":"another-login","authUrl":"https://www.workbuddy.cn/login?state=another-login"}})));
    let started = run(
        &runtime,
        &plugin,
        "cn",
        OperationInput::Auth {
            provider: provider.clone(),
            request: AuthRequest {
                step: AuthStep::Start {
                    redirect_uri: String::new(),
                    state: "host-state".into(),
                },
            },
        },
        local.scope(),
    )
    .await
    .unwrap();
    assert!(matches!(
        started,
        OperationOutput::Auth(AuthResponse::Authorization { .. })
    ));
    local.reply(Reply::json(json!({"code":11217})));
    let pending = run(
        &runtime,
        &plugin,
        "cn",
        OperationInput::Auth {
            provider: provider.clone(),
            request: AuthRequest {
                step: AuthStep::Poll,
            },
        },
        local.scope(),
    )
    .await
    .unwrap();
    assert!(matches!(
        pending,
        OperationOutput::Auth(AuthResponse::Pending { .. })
    ));

    local.reply(Reply::stream(&completion_stream(
        "hy3-x",
        "paid during pending login",
    )));
    let output = run(
        &runtime,
        &plugin,
        "cn",
        OperationInput::Infer {
            provider: provider.clone(),
            request: request(),
        },
        local.scope(),
    )
    .await
    .unwrap();
    assert_eq!(
        response_wire(output)["choices"][0]["message"]["content"],
        "paid during pending login"
    );
    assert_eq!(requested_models(&local).last().unwrap(), "hy3-x");

    local.reply(Reply::json(json!({"code":0,"data":{"accessToken":token("www.workbuddy.cn","account-a",2),"refreshToken":"new-refresh","domain":"www.workbuddy.cn"}})));
    let authenticated = run(
        &runtime,
        &plugin,
        "cn",
        OperationInput::Auth {
            provider: provider.clone(),
            request: AuthRequest {
                step: AuthStep::Poll,
            },
        },
        local.scope(),
    )
    .await
    .unwrap();
    let OperationOutput::Auth(AuthResponse::Credentials { values, .. }) = authenticated else {
        panic!("expected successful login");
    };
    provider.credentials = values;
    local.reply(Reply::stream(&completion_stream(
        "hy3-x",
        "paid after login",
    )));
    let output = run(
        &runtime,
        &plugin,
        "cn",
        OperationInput::Infer {
            provider: provider.clone(),
            request: request(),
        },
        local.scope(),
    )
    .await
    .unwrap();
    assert_eq!(
        response_wire(output)["choices"][0]["message"]["content"],
        "paid after login"
    );
    assert_eq!(requested_models(&local).last().unwrap(), "hy3-x");

    let revoked = run(
        &runtime,
        &plugin,
        "cn",
        OperationInput::Auth {
            provider: provider.clone(),
            request: AuthRequest {
                step: AuthStep::Revoke,
            },
        },
        local.scope(),
    )
    .await
    .unwrap();
    assert!(matches!(
        revoked,
        OperationOutput::Auth(AuthResponse::Revoked)
    ));
    local.reply(Reply::stream(&completion_stream(
        "hy3",
        "free after revoke",
    )));
    let output = run(
        &runtime,
        &plugin,
        "cn",
        OperationInput::Infer {
            provider,
            request: request(),
        },
        local.scope(),
    )
    .await
    .unwrap();
    assert_eq!(
        response_wire(output)["choices"][0]["message"]["content"],
        "free after revoke"
    );
    assert_eq!(requested_models(&local).last().unwrap(), "hy3");
    assert!(local.replies.lock().is_empty());
    println!(
        "text resetAt: paid window coexists with pending login, survives login completion, and is removed by revoke"
    );
}

#[tokio::test]
#[ignore = "build the release wasm32-wasip2 component first"]
async fn image_downloads_reject_unapproved_origins_and_never_forward_credentials() {
    let bytes = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/wasm32-wasip2/release/stravia_vendor_workbuddy.wasm"),
    )
    .unwrap();
    let runtime = VendorRuntime::new().unwrap();
    let plugin = runtime.load(&bytes).await.unwrap();
    let png = STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=").unwrap();
    let image_request = || stravia_vendor_sdk::MediaImageRequest {
        prompt: "蓝色圆形".into(),
        references: Vec::new(),
        aspect_ratio: None,
        resolution: None,
    };
    for (region, other, domain, cdn) in [
        (
            "cn",
            "intl",
            "www.workbuddy.cn",
            "https://images-1250000001.cos.ap-shanghai.myqcloud.com",
        ),
        (
            "intl",
            "cn",
            "www.workbuddy.ai",
            "https://images-1250000002.cos.ap-singapore.myqcloud.com",
        ),
    ] {
        let origin = format!("https://{domain}");
        let local = LocalUpstream::new(&[&origin, cdn]);
        let mut provider = snapshot(region, &origin);
        provider.model = Some("image-model".into());
        provider.credentials = BTreeMap::from([
            ("region".into(), json!(region)),
            ("domain".into(), json!(domain)),
            ("uid".into(), json!("image-account")),
            (
                "access_token".into(),
                json!(token(domain, "image-account", 1)),
            ),
        ]);
        for hostile in [
            "https://127.0.0.1/private",
            "https://evil.test/image.png",
            "https://images-1250000001.cos.ap-shanghai.myqcloud.com.evil.test/image.png",
            "https://images-1250000001.cos.ap-shanghai.myqcloud.com:444/image.png",
            if region == "cn" {
                "https://www.workbuddy.ai/image.png"
            } else {
                "https://www.workbuddy.cn/image.png"
            },
        ] {
            local.reply(Reply::json(
                json!({"code":0,"data":{"data":[{"url":hostile}]}}),
            ));
            let before = local.requests.lock().len();
            assert!(
                run(
                    &runtime,
                    &plugin,
                    region,
                    OperationInput::MediaImage {
                        provider: provider.clone(),
                        request: image_request(),
                    },
                    local.scope(),
                )
                .await
                .is_err()
            );
            assert_eq!(local.requests.lock().len(), before + 1);
            assert!(local.replies.lock().is_empty());
        }
        local.reply(Reply::json(json!({
            "code":0,
            "data":{"data":[{"url":format!("{cdn}/image.png?signature=synthetic")}],
                    "usage":{"input_tokens":346,"output_tokens":464,"total_tokens":810,"credit":5.71}}
        })));
        local.reply(Reply {
            status: 200,
            headers: vec![("Content-Type".into(), "image/png".into())],
            chunks: Mutex::new(VecDeque::from([png.clone()])),
        });
        let output = run(
            &runtime,
            &plugin,
            region,
            OperationInput::MediaImage {
                provider: provider.clone(),
                request: image_request(),
            },
            local.scope(),
        )
        .await
        .unwrap();
        let OperationOutput::MediaImage(output) = output else {
            panic!("expected image output");
        };
        assert_eq!(output.artifacts[0].bytes, png);
        assert!(output.artifacts[0].upstream_ref.is_none());
        assert_eq!(output.usage.unwrap().total_tokens, 810);
        {
            let requests = local.requests.lock();
            let download = requests.last().unwrap();
            assert_eq!(download.method, "GET");
            assert_eq!(download.headers, [("Accept".into(), "image/*".into())]);
            assert!(download.body.is_empty());
        }
        provider.credentials.insert("region".into(), json!(other));
        let before = local.requests.lock().len();
        assert!(
            run(
                &runtime,
                &plugin,
                region,
                OperationInput::MediaImage {
                    provider,
                    request: image_request(),
                },
                local.scope(),
            )
            .await
            .is_err()
        );
        assert_eq!(local.requests.lock().len(), before);
    }
}
