use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use parking_lot::Mutex;
use serde_json::{Value, json};
use stravia_protocol_codec::transform::ProtocolTransform;
use stravia_runtime_contract::{CancellationToken, Deadline};
use stravia_vendor_common::common;
use stravia_vendor_runtime::{
    HostFailure, HostHttpResponse, HostServices, HostWebSocket, HttpRequest, LogLevel,
    OperationScope, RuntimeEvent, VendorRuntime,
};
use stravia_vendor_sdk::{
    AllowanceRequest, AuthRequest, AuthResponse, AuthStep, DiscoverRequest, ErrorKind,
    OperationInput, OperationOutput, ProviderSnapshot,
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
        let url = url::Url::parse(&request.url).unwrap();
        assert!(
            self.origins.contains(&url.origin().ascii_serialization()),
            "HTTP destination must be declared and approved"
        );
        self.requests.lock().push(request);
        Ok(Arc::new(
            self.replies
                .lock()
                .pop_front()
                .expect("unexpected HTTP request or retry"),
        ))
    }
    async fn ws_connect(
        &self,
        _: String,
        _: Vec<(String, String)>,
        _: Vec<String>,
        _: Option<String>,
    ) -> Result<Arc<dyn HostWebSocket>, HostFailure> {
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

fn whitelist_request() -> stravia_vendor_sdk::AiRequest {
    let endpoint = common::endpoint("openai-compatible").unwrap();
    let mut request = ProtocolTransform::global()
        .bind(endpoint, endpoint)
        .unwrap()
        .decode_request(json!({
            "model":"client-route-name", "stream":true,
            "temperature":0.25,"top_p":0.8,"max_tokens":123,"reasoning_effort":"medium",
            "store":true,"prompt_cache_retention":"24h",
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
    // 宿主完成路由后会选择 Target Thinking Control；codec 不直接编码 ingress effort。
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
        .encode_request(&request())
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
    for (region, domain) in [("cn", "copilot.tencent.com"), ("intl", "www.workbuddy.ai")] {
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
            .encode_request(&input)
            .unwrap()
            .body;
        local.reply(Reply::stream(&completion_stream(
            "test-model",
            "whitelist answer",
        )));
        let output = runtime
            .execute(
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
        let body: Value = serde_json::from_slice(&chat.body).unwrap();
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

    for (region, domain) in [("cn", "copilot.tencent.com"), ("intl", "www.workbuddy.ai")] {
        let origin = format!("https://{domain}");
        let local = LocalUpstream::new(&[&origin]);
        let mut provider = snapshot(region, &origin);
        local.reply(Reply::json(json!({"code":0,"data":{"state":"local-state","authUrl":format!("{origin}/login?state=local-state")}})));
        let start = runtime
            .execute(
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

        local.reply(Reply::json(json!({"code":11217})));
        let pending = runtime
            .execute(
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
        let login = runtime
            .execute(
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
        let refresh = runtime
            .execute(
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

        local.reply(Reply::json(json!({"code":0,"data":{"models":[{"id":"test-model","name":"Test","supportsToolCall":true,"credits":"x0.10"}],"agents":[{"name":"cli","tags":["default"],"models":["test-model"]}]}})));
        let discovered = runtime
            .execute(
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
        let output = runtime
            .execute(
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
            let body: Value = serde_json::from_slice(&chat.body).unwrap();
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
        let rejected = runtime
            .execute(
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

        local.reply(Reply {
            status: 429,
            headers: vec![("retry-after".into(), "17".into())],
            chunks: Mutex::new(VecDeque::from([
                br#"{"error":{"message":"rate limited"}}"#.to_vec()
            ])),
        });
        let limited = runtime
            .execute(
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
        let truncated = runtime
            .execute(
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

fn resource_response(rows: Vec<Value>) -> Value {
    json!({"code":0,"data":{"Response":{"Data":{"Accounts":rows}}}})
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
    for (region, domain, billing) in [
        ("cn", "copilot.tencent.com", "https://www.codebuddy.cn"),
        ("intl", "www.workbuddy.ai", "https://www.workbuddy.ai"),
    ] {
        let origin = format!("https://{domain}");
        let mut allowed = vec![origin.clone()];
        for declared in &plugin.descriptor().providers[0].network.extra_origins {
            allowed.push(format!("{}://{}", declared.scheme, declared.host));
        }
        let local = LocalUpstream::new(&allowed.iter().map(String::as_str).collect::<Vec<_>>());
        let mut provider = snapshot(region, &origin);
        provider.credentials = BTreeMap::from([
            ("region".into(), json!(region)),
            ("domain".into(), json!(domain)),
            ("uid".into(), json!("quota-account")),
            (
                "access_token".into(),
                json!(token(domain, "quota-account", 1)),
            ),
        ]);
        let input = || OperationInput::Allowance {
            provider: provider.clone(),
            request: AllowanceRequest::default(),
        };
        local.reply(Reply::json(resource_response(vec![
            json!({
                "PackageName":"Monthly",
                "CycleCapacitySize":500, "CycleCapacityRemain":"334.16", "CycleCapacityUsed":165,
                "CapacitySize":5000, "CapacityRemain":4900, "CapacityUsed":100
            }),
            json!({
                "PackageName":"Rewards", "CycleCapacitySize":0,
                "CapacitySize":4481, "CapacityRemain":4481
            }),
        ])));
        let output = runtime
            .execute(&plugin, region, input(), local.scope())
            .await
            .unwrap();
        let OperationOutput::Allowance(result) = output else {
            panic!("expected allowance result")
        };
        assert_eq!(result.allowances.len(), 1);
        assert_eq!(
            result.allowances[0].remaining.as_ref().unwrap().value,
            "4815.16"
        );
        assert_eq!(result.allowances[0].used.as_ref().unwrap().value, "165.84");
        assert_eq!(result.allowances[0].limit.as_ref().unwrap().value, "4981");
        assert_eq!(result.allowances[0].used_percent.as_deref(), Some("3.33"));
        assert!(result.allowances[0].label.contains("4981"));
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
            let request = requests.last().unwrap();
            assert_eq!(
                request.url,
                format!("{billing}/v2/billing/meter/get-user-resource")
            );
            assert_eq!(request.method, "POST");
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["ProductCode"], "p_tcaca");
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
        }

        // 满页后继续读取，不把第一页余额当作完整账号额度。
        let page = (0..100)
            .map(|_| {
                json!({
                    "PackageName":"Rewards", "CapacityRemain":1, "CapacitySize":2, "CapacityUsed":1
                })
            })
            .collect();
        local.reply(Reply::json(resource_response(page)));
        local.reply(Reply::json(resource_response(vec![
            json!({"PackageName":"Last","CapacityRemain":7,"CapacitySize":10}),
        ])));
        let output = runtime
            .execute(&plugin, region, input(), local.scope())
            .await
            .unwrap();
        let OperationOutput::Allowance(result) = output else {
            panic!("expected paginated allowance")
        };
        assert_eq!(result.allowances.len(), 1);
        assert!(result.allowances[0].label.contains("210"));
        assert_eq!(result.allowances[0].used.as_ref().unwrap().value, "103");
        assert_eq!(
            result.allowances[0].remaining.as_ref().unwrap().value,
            "107"
        );
        {
            let requests = local.requests.lock();
            let page: Value = serde_json::from_slice(&requests.last().unwrap().body).unwrap();
            assert_eq!(page["PageNumber"], 2);
        }

        local.reply(Reply::json(resource_response(
            (0..100)
                .map(|_| json!({"CapacityRemain":1,"CapacitySize":2}))
                .collect(),
        )));
        local.reply(Reply::json(json!({"code":123})));
        assert!(
            runtime
                .execute(&plugin, region, input(), local.scope())
                .await
                .is_err(),
            "a later-page failure must not publish the first page as the account balance"
        );

        local.reply(Reply::json(resource_response(Vec::new())));
        let output = runtime
            .execute(&plugin, region, input(), local.scope())
            .await
            .unwrap();
        let OperationOutput::Allowance(result) = output else {
            panic!("expected explicit empty balance")
        };
        assert_eq!(result.allowances[0].remaining.as_ref().unwrap().value, "0");
        assert_eq!(result.allowances[0].used.as_ref().unwrap().value, "0");

        local.reply(Reply::json(json!({"code":123,"msg":"do-not-echo"})));
        let error = runtime
            .execute(&plugin, region, input(), local.scope())
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
            runtime
                .execute(&plugin, region, input(), local.scope())
                .await
                .is_err()
        );

        local.reply(Reply {
            status: 429,
            headers: vec![("retry-after".into(), "17".into())],
            chunks: Mutex::new(VecDeque::from([b"{}".to_vec()])),
        });
        let limited = runtime
            .execute(&plugin, region, input(), local.scope())
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
            runtime
                .execute(
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
            "{region}: single-row credit totals, decimal usage, pagination, zero balance, upstream failures and credential isolation passed"
        );
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
    let local = LocalUpstream::new(&["https://copilot.tencent.com"]);
    let mut provider = snapshot("cn", "https://copilot.tencent.com");
    provider.credentials = BTreeMap::from([
        ("region".into(), json!("cn")),
        ("domain".into(), json!("copilot.tencent.com")),
        ("uid".into(), json!("account-a")),
        (
            "access_token".into(),
            json!(token("copilot.tencent.com", "account-a", 1)),
        ),
    ]);
    local.reply(Reply::json(json!({"code":0,"data":{
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
    let output = runtime
        .execute(
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
        let result = runtime
            .execute(
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
    println!(
        "default agent: historical models excluded, pair IDs merged, same-name models retained, invalid rosters refused"
    );
}

fn paid_provider() -> ProviderSnapshot {
    let mut provider = snapshot("cn", "https://copilot.tencent.com");
    provider.model = Some("hy3".into());
    provider.credentials = BTreeMap::from([
        ("region".into(), json!("cn")),
        ("domain".into(), json!("copilot.tencent.com")),
        ("uid".into(), json!("account-a")),
        (
            "access_token".into(),
            json!(token("copilot.tencent.com", "account-a", 1)),
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
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let model = body["model"].as_str().unwrap();
            model.to_owned()
        })
        .collect()
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
        let local =
            LocalUpstream::new(&["https://copilot.tencent.com", "https://www.workbuddy.ai"]);
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
        let output = runtime
            .execute(
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
            let body: Value = serde_json::from_slice(&attempt.body).unwrap();
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
        let output = runtime
            .execute(
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
            let output = runtime
                .execute(
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
                json!(token("copilot.tencent.com", "account-b", 2)),
            );
            local.reply(Reply::stream(&completion_stream(
                "hy3",
                "other account free",
            )));
            let output = runtime
                .execute(
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
            let output = runtime
                .execute(
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
            let output = runtime
                .execute(
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
            let output = runtime
                .execute(
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
        let local = LocalUpstream::new(&["https://copilot.tencent.com"]);
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
        let result = runtime
            .execute(
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
    let local = LocalUpstream::new(&["https://copilot.tencent.com"]);
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
    let output = runtime
        .execute(
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
    let output = runtime
        .execute(
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
    let local = LocalUpstream::new(&["https://copilot.tencent.com"]);
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
    let output = runtime
        .execute(
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

    local.reply(Reply::json(json!({"code":0,"data":{"state":"another-login","authUrl":"https://copilot.tencent.com/login?state=another-login"}})));
    let started = runtime
        .execute(
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
    let pending = runtime
        .execute(
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
    let output = runtime
        .execute(
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

    local.reply(Reply::json(json!({"code":0,"data":{"accessToken":token("copilot.tencent.com","account-a",2),"refreshToken":"new-refresh","domain":"copilot.tencent.com"}})));
    let authenticated = runtime
        .execute(
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
    let output = runtime
        .execute(
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

    let revoked = runtime
        .execute(
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
    let output = runtime
        .execute(
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
