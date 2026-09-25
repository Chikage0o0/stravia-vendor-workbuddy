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

        local.reply(Reply::json(json!({"code":0,"data":{"models":[{"id":"test-model","name":"Test","supportsToolCall":true,"credits":"x0.10"}]}})));
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
        assert!(discovered.models.iter().any(|model| model.id == "test-model"));

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
        assert_eq!(
            result.allowances[0].limit.as_ref().unwrap().value,
            "4981"
        );
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
