//! WorkBuddy 浏览器登录（Start/Poll/Refresh/Revoke）与区域凭据。
//!
//! 隔离边界：
//! - 所有 HTTP 只发往当前 channel 对应区域的固定 origin（`lib.rs` 的 `admit`
//!   已校验 `provider.base_url` 与 channel 一致）。
//! - 凭据内持久化 `region`/`domain` 标签；刷新与出站标头生成前复核，区域
//!   不一致在任何 HTTP 发出之前失败。
//! - 待登录状态写入宿主私有状态，仅保存 `{state, region, created}`，不缓存
//!   任何上游端点，Poll 地址永远由当前区域推导。
//! - 错误消息不携带令牌、state 或可能回显凭据的上游文案。

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use stravia_vendor_common::common;
use stravia_vendor_sdk::{
    AuthRequest, AuthResponse, AuthStep, ErrorKind, GuestHost, HttpRequest, PluginError,
    ProviderSnapshot, read_http_body,
};

use crate::{Region, VENDOR_ID};

/// 发起浏览器登录，返回 `{state, authUrl}`。
const AUTH_STATE_PATH: &str = "/v2/plugin/auth/state";
/// 浏览器授权完成后换取令牌。
const AUTH_TOKEN_PATH: &str = "/v2/plugin/auth/token";
/// 刷新并轮换令牌。
const AUTH_REFRESH_PATH: &str = "/v2/plugin/auth/token/refresh";
/// 上游唯一接受的 `platform` 取值。
const PLATFORM: &str = "CLI";
/// 业务错误码：浏览器尚未完成授权。
const LOGIN_PENDING: i64 = 11217;
/// 登录窗口有效期。
const LOGIN_TTL_MS: i64 = 600_000;
/// 建议的宿主轮询间隔。
const POLL_INTERVAL_SECONDS: u32 = 3;
/// 认证端点响应体上限。
const MAX_AUTH_BODY: usize = 256 * 1024;
/// 私有状态结构版本。
const STATE_VERSION: u32 = 1;

/// 写入宿主私有状态的待登录会话。
#[derive(Debug, Serialize, Deserialize)]
struct PendingLogin {
    version: u32,
    /// 上游签发的浏览器登录 state（非宿主回调 state）。
    state: String,
    /// 发起登录时的区域标签，Poll 前复核。
    region: String,
    created_unix_ms: i64,
}

/// 从 `ProviderSnapshot.credentials` 读出的已校验凭据。
struct StoredCredentials {
    access_token: Option<String>,
    refresh_token: Option<String>,
    uid: String,
    domain: String,
    enterprise_id: Option<String>,
}

pub(crate) fn execute(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    region: Region,
    request: AuthRequest,
) -> Result<AuthResponse, PluginError> {
    match request.step {
        AuthStep::Start { .. } => start(host, region),
        AuthStep::Poll => poll(host, region),
        AuthStep::Refresh => refresh(host, provider, region),
        AuthStep::Revoke => revoke(host),
        // 授权码回跳与手动输入都不属于该浏览器登录流程。
        AuthStep::Exchange { .. } | AuthStep::ManualInput { .. } => Err(common::unsupported(
            "auth exchange/manual-input",
            VENDOR_ID,
            region.id(),
        )),
    }
}

/// 发起登录：POST /v2/plugin/auth/state，保存待登录 state，返回授权地址。
fn start(host: &GuestHost, region: Region) -> Result<AuthResponse, PluginError> {
    let url = format!("{}{}?platform={PLATFORM}", region.origin(), AUTH_STATE_PATH);
    let payload = json_request(
        host,
        "POST",
        &url,
        auth_endpoint_headers(region),
        b"{}".to_vec(),
    )?;
    reject_nonzero_code(&payload)?;
    let data = payload.get("data").unwrap_or(&Value::Null);
    let state = required_str(data, "state")?;
    let auth_url = required_str(data, "authUrl")?;
    validate_authorization_url(auth_url)?;
    write_pending(
        host,
        &PendingLogin {
            version: STATE_VERSION,
            state: state.to_owned(),
            region: region.id().to_owned(),
            created_unix_ms: unix_millis(),
        },
    )?;
    Ok(AuthResponse::Authorization {
        url: auth_url.to_owned(),
        user_code: None,
        verification_uri: Some(auth_url.to_owned()),
        interval_seconds: Some(POLL_INTERVAL_SECONDS),
        state: None,
    })
}

/// 轮询授权结果：GET /v2/plugin/auth/token?state=…。
/// 仅 code==11217 视为等待中；其余非零 code 与畸形响应直接失败。
fn poll(host: &GuestHost, region: Region) -> Result<AuthResponse, PluginError> {
    let pending = read_pending(host)?
        .ok_or_else(|| auth_error("WorkBuddy login session is missing; start again"))?;
    if pending.version != STATE_VERSION {
        return Err(invalid(
            "stored WorkBuddy login state has an unsupported version",
        ));
    }
    // 区域标签是主隔离边界：待登录 state 属于哪个区域只能在哪个区域兑换。
    if pending.region != region.id() {
        return Err(auth_error(
            "pending WorkBuddy login belongs to another region",
        ));
    }
    if unix_millis().saturating_sub(pending.created_unix_ms) > LOGIN_TTL_MS {
        clear_pending(host)?;
        return Err(auth_error("WorkBuddy login window expired; start again"));
    }
    let state_query: String =
        url::form_urlencoded::byte_serialize(pending.state.as_bytes()).collect();
    let url = format!("{}{}?state={state_query}", region.origin(), AUTH_TOKEN_PATH);
    let payload = json_request(host, "GET", &url, auth_endpoint_headers(region), Vec::new())?;
    let code = payload
        .get("code")
        .and_then(Value::as_i64)
        .ok_or_else(|| malformed("auth/token response is missing `code`"))?;
    if code == LOGIN_PENDING {
        return Ok(AuthResponse::Pending {
            retry_after_seconds: Some(POLL_INTERVAL_SECONDS),
        });
    }
    if code != 0 {
        return Err(auth_error(format!(
            "WorkBuddy login was rejected: code={code}"
        )));
    }
    let data = payload.get("data").unwrap_or(&Value::Null);
    let access_token = required_str(data, "accessToken")?.to_owned();
    let credentials = credentials_from_token_data(region, data, &access_token)?;
    clear_pending(host)?;
    Ok(credentials)
}

/// 刷新：POST /v2/plugin/auth/token/refresh。
/// 上游轮换 refresh token，新值缺失时保留旧值；强制校验 uid 与区域不变。
fn refresh(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    region: Region,
) -> Result<AuthResponse, PluginError> {
    let stored = stored_credentials(provider, region)?;
    let refresh_token = stored
        .refresh_token
        .clone()
        .ok_or_else(|| auth_error("WorkBuddy refresh token is missing"))?;
    let mut request_headers = auth_endpoint_headers(region);
    request_headers.extend([
        ("X-Refresh-Token".to_owned(), refresh_token.clone()),
        (
            "X-Auth-Refresh-Source".to_owned(),
            match region {
                Region::Cn => "workbuddy".to_owned(),
                Region::Intl => "plugin".to_owned(),
            },
        ),
        ("X-User-Id".to_owned(), stored.uid.clone()),
        ("X-Domain".to_owned(), stored.domain.clone()),
        ("X-CodeBuddy-Request".to_owned(), "1".to_owned()),
        (
            "Accept-Language".to_owned(),
            accept_language(region).to_owned(),
        ),
    ]);
    if let Some(enterprise_id) = &stored.enterprise_id {
        request_headers.push(("X-Enterprise-Id".to_owned(), enterprise_id.clone()));
    }
    let url = format!("{}{}", region.origin(), AUTH_REFRESH_PATH);
    let payload = json_request(host, "POST", &url, request_headers, b"{}".to_vec())?;
    reject_nonzero_code(&payload)?;
    // 响应兼容双层包裹：data.data 为对象时优先取内层。
    let outer = payload.get("data").unwrap_or(&Value::Null);
    let data = outer
        .get("data")
        .filter(|value| value.is_object())
        .unwrap_or(outer);
    let access_token = data
        .get("accessToken")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| auth_error("WorkBuddy token refresh returned no access token"))?
        .to_owned();
    // 身份稳定性：新令牌必须仍属于同一个账号。
    let claims = jwt_claims(&access_token)
        .ok_or_else(|| auth_error("refreshed WorkBuddy access token is not a decodable JWT"))?;
    let new_uid = claims
        .get("sub")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| auth_error("refreshed WorkBuddy access token carries no subject"))?;
    if new_uid != stored.uid {
        return Err(auth_error(
            "refreshed WorkBuddy credential changed account identity",
        ));
    }
    let issuer = claims
        .get("iss")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let response_domain = data
        .get("domain")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty());
    if contradicts_region(region, issuer, response_domain) {
        return Err(auth_error(
            "refreshed WorkBuddy credential belongs to the other region",
        ));
    }
    let domain = response_domain.unwrap_or(stored.domain.as_str()).to_owned();
    // 上游轮换 refresh token：新值缺失时沿用旧值。
    let next_refresh = data
        .get("refreshToken")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(refresh_token.as_str());
    let expires_at_unix_ms =
        normalize_epoch_ms(data.get("expiresAt")).or_else(|| jwt_exp_unix_ms(&access_token));
    let values = credential_values(
        region,
        &stored.uid,
        &domain,
        &access_token,
        Some(next_refresh),
        stored.enterprise_id.as_deref(),
    )?;
    Ok(AuthResponse::Credentials {
        values,
        expires_at_unix_ms,
    })
}

/// 吊销：无已知的上游吊销端点，清除本地待登录状态与付费窗口并报告 Revoked。
fn revoke(host: &GuestHost) -> Result<AuthResponse, PluginError> {
    host.write_private_state(b"")?;
    Ok(AuthResponse::Revoked)
}

/// 供推理与模型发现共用的完整出站标头（桌面端产品身份 + 请求信封）。
/// 凭据字段先校验再进入标头，控制字符/空白一律拒绝，防止标头注入。
pub(crate) fn headers(
    provider: &ProviderSnapshot,
    region: Region,
) -> Result<Vec<(String, String)>, PluginError> {
    let stored = stored_credentials(provider, region)?;
    let access_token = stored
        .access_token
        .ok_or_else(|| auth_error("WorkBuddy access token is missing"))?;
    let mut headers = vec![
        ("Content-Type".to_owned(), "application/json".to_owned()),
        (
            "Accept".to_owned(),
            "application/json, text/plain, */*".to_owned(),
        ),
        ("X-Requested-With".to_owned(), "XMLHttpRequest".to_owned()),
        ("Origin".to_owned(), region.website().to_owned()),
        ("Referer".to_owned(), format!("{}/", region.website())),
        ("X-CodeBuddy-Request".to_owned(), "1".to_owned()),
        (
            "Accept-Language".to_owned(),
            accept_language(region).to_owned(),
        ),
        // 桌面端产品身份标头。
        ("X-Agent-Purpose".to_owned(), "conversation".to_owned()),
        ("X-IDE-Name".to_owned(), "WorkBuddy".to_owned()),
        ("X-IDE-Type".to_owned(), "WorkBuddy".to_owned()),
        ("X-IDE-Version".to_owned(), region.version().to_owned()),
        ("X-Product".to_owned(), "WorkBuddy".to_owned()),
        ("X-Domain".to_owned(), stored.domain),
        ("User-Agent".to_owned(), region.user_agent().to_owned()),
        ("X-User-Id".to_owned(), stored.uid),
        ("Authorization".to_owned(), format!("Bearer {access_token}")),
        // 每个请求一枚随机 UUID（32 位十六进制，无连字符）。
        (
            "X-Request-ID".to_owned(),
            uuid::Uuid::new_v4().simple().to_string(),
        ),
    ];
    if let Some(enterprise_id) = stored.enterprise_id {
        headers.push(("X-Enterprise-Id".to_owned(), enterprise_id.clone()));
        headers.push(("X-Tenant-Id".to_owned(), enterprise_id));
    }
    Ok(headers)
}

/// 额度查询与聊天共用凭据校验，但使用计费接口要求的轻量请求标头。
pub(crate) fn billing_headers(
    provider: &ProviderSnapshot,
    region: Region,
) -> Result<Vec<(String, String)>, PluginError> {
    let stored = stored_credentials(provider, region)?;
    let token = stored
        .access_token
        .ok_or_else(|| auth_error("WorkBuddy access token is missing"))?;
    // 计费接口使用网页侧标头，不使用对话接口的 WorkBuddy IDE 身份。
    let mut headers = auth_endpoint_headers(region);
    headers.extend([
        ("Authorization".into(), format!("Bearer {token}")),
        ("X-User-Id".into(), stored.uid),
        ("X-Domain".into(), stored.domain),
        ("X-CodeBuddy-Request".into(), "1".into()),
        ("Accept-Language".into(), accept_language(region).into()),
        (
            "X-Request-ID".into(),
            uuid::Uuid::new_v4().simple().to_string(),
        ),
    ]);
    if let Some(id) = stored.enterprise_id {
        headers.push(("X-Enterprise-Id".into(), id.clone()));
        headers.push(("X-Tenant-Id".into(), id));
    } else {
        headers.push(("X-No-Enterprise-Id".into(), "1".into()));
    }
    if region == Region::Cn {
        headers.push(("X-Product".into(), "SaaS".into()));
    }
    Ok(headers)
}

/// 从 auth/token 的 `data` 构造 Credentials 响应。
fn credentials_from_token_data(
    region: Region,
    data: &Value,
    access_token: &str,
) -> Result<AuthResponse, PluginError> {
    // JWT 只做 base64url 解码取元数据，签名校验仍由上游在请求时完成。
    let claims = jwt_claims(access_token)
        .ok_or_else(|| auth_error("WorkBuddy access token is not a decodable JWT"))?;
    let uid = claims
        .get("sub")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| auth_error("WorkBuddy access token carries no subject"))?;
    let issuer = claims
        .get("iss")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let upstream_domain = data
        .get("domain")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if contradicts_region(region, issuer, upstream_domain) {
        return Err(auth_error(
            "WorkBuddy login returned a credential for the other region",
        ));
    }
    let domain = upstream_domain.unwrap_or_else(|| region.domain());
    let refresh_token = data
        .get("refreshToken")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let enterprise_id = data
        .get("enterpriseId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let expires_at_unix_ms =
        normalize_epoch_ms(data.get("expiresAt")).or_else(|| jwt_exp_unix_ms(access_token));
    let values = credential_values(
        region,
        uid,
        domain,
        access_token,
        refresh_token,
        enterprise_id,
    )?;
    Ok(AuthResponse::Credentials {
        values,
        expires_at_unix_ms,
    })
}

/// 组装持久化凭据键集合。宿主以 `subject_id` 作为账户元数据，`uid` 保留给
/// 本插件的身份校验；`region`/`domain` 标签供跨区域隔离复核。
fn credential_values(
    region: Region,
    uid: &str,
    domain: &str,
    access_token: &str,
    refresh_token: Option<&str>,
    enterprise_id: Option<&str>,
) -> Result<BTreeMap<String, Value>, PluginError> {
    for (field, value) in [
        ("uid", Some(uid)),
        ("domain", Some(domain)),
        ("access_token", Some(access_token)),
        ("refresh_token", refresh_token),
        ("enterprise_id", enterprise_id),
    ] {
        if let Some(value) = value {
            check_header_safe(field, value)?;
        }
    }
    let mut values = BTreeMap::new();
    values.insert(
        "access_token".into(),
        Value::String(access_token.to_owned()),
    );
    if let Some(refresh_token) = refresh_token {
        values.insert(
            "refresh_token".into(),
            Value::String(refresh_token.to_owned()),
        );
    }
    values.insert("uid".into(), Value::String(uid.to_owned()));
    values.insert("subject_id".into(), Value::String(uid.to_owned()));
    values.insert("domain".into(), Value::String(domain.to_owned()));
    values.insert("region".into(), Value::String(region.id().to_owned()));
    if let Some(enterprise_id) = enterprise_id {
        values.insert(
            "enterprise_id".into(),
            Value::String(enterprise_id.to_owned()),
        );
    }
    Ok(values)
}

/// 读取并校验已保存凭据：region 标签必须等于当前区域，domain 不得属于对侧
/// 区域，所有将进入标头的字段必须通过注入检查。
fn stored_credentials(
    provider: &ProviderSnapshot,
    region: Region,
) -> Result<StoredCredentials, PluginError> {
    match credential_str(provider, "region") {
        Some(tag) if tag == region.id() => {}
        _ => {
            return Err(auth_error(
                "stored credential region does not match the selected WorkBuddy channel",
            ));
        }
    }
    let domain = credential_str(provider, "domain")
        .unwrap_or_else(|| region.domain())
        .to_owned();
    if domain_contradicts(&domain, region) {
        return Err(auth_error(
            "stored credential domain belongs to the other WorkBuddy region",
        ));
    }
    let uid = credential_str(provider, "uid")
        .ok_or_else(|| auth_error("stored WorkBuddy credential is missing `uid`"))?
        .to_owned();
    let access_token = credential_str(provider, "access_token").map(str::to_owned);
    let refresh_token = credential_str(provider, "refresh_token").map(str::to_owned);
    let enterprise_id = credential_str(provider, "enterprise_id").map(str::to_owned);
    for (field, value) in [
        ("uid", Some(uid.as_str())),
        ("domain", Some(domain.as_str())),
        ("access_token", access_token.as_deref()),
        ("refresh_token", refresh_token.as_deref()),
        ("enterprise_id", enterprise_id.as_deref()),
    ] {
        if let Some(value) = value {
            check_header_safe(field, value)?;
        }
    }
    Ok(StoredCredentials {
        access_token,
        refresh_token,
        uid,
        domain,
        enterprise_id,
    })
}

fn credential_str<'a>(provider: &'a ProviderSnapshot, key: &str) -> Option<&'a str> {
    provider
        .credentials
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// 标头值只允许可见 ASCII（0x21..=0x7e），拒绝空白与控制字符注入。
fn check_header_safe(field: &str, value: &str) -> Result<(), PluginError> {
    if value.is_empty() || !value.bytes().all(|byte| (0x21..=0x7e).contains(&byte)) {
        return Err(auth_error(format!(
            "stored credential `{field}` is not safe for HTTP headers"
        )));
    }
    Ok(())
}

/// 授权地址只允许无内嵌凭据的 HTTPS。
fn validate_authorization_url(value: &str) -> Result<(), PluginError> {
    let url = url::Url::parse(value)
        .map_err(|_| malformed("auth/state returned an invalid authorization URL"))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(malformed(
            "authorization URL must be plain HTTPS without embedded credentials",
        ));
    }
    Ok(())
}

/// 只识别已知域名子串：未知 issuer 只是
/// 元数据，只有命中对侧区域的已知域名才算矛盾证据。
fn contradicts_region(region: Region, issuer: &str, domain: Option<&str>) -> bool {
    let issuer = issuer.to_ascii_lowercase();
    let domain = domain.unwrap_or_default().to_ascii_lowercase();
    let hits = |markers: &[&str]| {
        markers
            .iter()
            .any(|marker| issuer.contains(marker) || domain.contains(marker))
    };
    match region {
        Region::Cn => hits(&["workbuddy.ai"]),
        Region::Intl => hits(&["copilot.tencent.com", "codebuddy.cn", "workbuddy.cn"]),
    }
}

fn domain_contradicts(domain: &str, region: Region) -> bool {
    contradicts_region(region, "", Some(domain))
}

/// 认证端点共享的请求信封。
fn auth_endpoint_headers(region: Region) -> Vec<(String, String)> {
    vec![
        ("Content-Type".to_owned(), "application/json".to_owned()),
        (
            "Accept".to_owned(),
            "application/json, text/plain, */*".to_owned(),
        ),
        ("X-Requested-With".to_owned(), "XMLHttpRequest".to_owned()),
        (
            "User-Agent".to_owned(),
            format!("WorkBuddy/{}", region.version()),
        ),
        ("Origin".to_owned(), region.website().to_owned()),
        ("Referer".to_owned(), format!("{}/", region.website())),
    ]
}

fn accept_language(region: Region) -> &'static str {
    match region {
        Region::Cn => "zh-CN",
        Region::Intl => "en-US",
    }
}

/// 经宿主受控通道的 JSON 请求；非 2xx 交给 `common::upstream_error` 分类。
fn json_request(
    host: &GuestHost,
    method: &str,
    url: &str,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
) -> Result<Value, PluginError> {
    let response = host.http_start(HttpRequest {
        method: method.to_owned(),
        url: url.to_owned(),
        headers,
        body,
    })?;
    let status = response.status()?;
    let response_headers = response.headers()?;
    let bytes = read_http_body(&response, MAX_AUTH_BODY)?;
    if !(200..300).contains(&status) {
        let mut error = common::upstream_error(status, &response_headers, &bytes);
        error.message = format!("WorkBuddy authentication HTTP {status}");
        return Err(error);
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| malformed("WorkBuddy upstream returned invalid JSON"))
}

/// `code` 字段存在时必须为零（start/refresh 路径；poll 单独区分 11217）。
fn reject_nonzero_code(payload: &Value) -> Result<(), PluginError> {
    if let Some(value) = payload.get("code") {
        let code = value
            .as_i64()
            .ok_or_else(|| malformed("WorkBuddy auth response has an invalid code"))?;
        if code != 0 {
            return Err(auth_error(format!(
                "WorkBuddy auth request was rejected: code={code}"
            )));
        }
    }
    Ok(())
}

fn required_str<'a>(value: &'a Value, key: &str) -> Result<&'a str, PluginError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| malformed(format!("WorkBuddy upstream response is missing `{key}`")))
}

/// base64url 解码 JWT 载荷；仅作元数据使用，签名校验仍在上游侧。
fn jwt_claims(token: &str) -> Option<Value> {
    let segment = token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(segment)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(segment))
        .ok()?;
    serde_json::from_slice(&decoded).ok()
}

fn jwt_exp_unix_ms(token: &str) -> Option<i64> {
    let exp = jwt_claims(token)?.get("exp")?.as_i64()?;
    if exp <= 0 {
        return None;
    }
    Some(exp.saturating_mul(1000))
}

/// `expiresAt` 可能是秒或毫秒，>1e11 视为毫秒。
fn normalize_epoch_ms(value: Option<&Value>) -> Option<i64> {
    let raw = match value? {
        Value::Number(number) => number.as_f64()?,
        Value::String(text) => text.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    if !raw.is_finite() || raw <= 0.0 {
        return None;
    }
    let seconds = if raw > 1e11 { raw / 1000.0 } else { raw };
    Some((seconds as i64).saturating_mul(1000))
}

fn read_pending(host: &GuestHost) -> Result<Option<PendingLogin>, PluginError> {
    let root = crate::state::read(host)?;
    if !root.contains_key("state") {
        return Ok(None);
    }
    serde_json::from_value(Value::Object(root))
        .map(Some)
        .map_err(|_| invalid("stored WorkBuddy login state is malformed"))
}

fn write_pending(host: &GuestHost, pending: &PendingLogin) -> Result<(), PluginError> {
    let mut root = crate::state::read(host)?;
    let Value::Object(value) = serde_json::to_value(pending)
        .map_err(|_| invalid("WorkBuddy login state could not be encoded"))?
    else {
        return Err(invalid("WorkBuddy login state could not be encoded"));
    };
    root.extend(value);
    crate::state::write(host, root)
}

/// 清除旧 root 格式的认证字段，保留付费窗口与未知扩展。
fn clear_pending(host: &GuestHost) -> Result<(), PluginError> {
    let mut root = crate::state::read(host)?;
    for key in ["version", "state", "region", "created_unix_ms"] {
        root.remove(key);
    }
    crate::state::write(host, root)
}

fn unix_millis() -> i64 {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
}

fn invalid(message: impl Into<String>) -> PluginError {
    common::plugin_error(ErrorKind::Invalid, message)
}

fn auth_error(message: impl Into<String>) -> PluginError {
    common::plugin_error(ErrorKind::Auth, message)
}

fn malformed(message: impl Into<String>) -> PluginError {
    common::plugin_error(ErrorKind::upstream_unknown(), message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(payload: &str) -> String {
        let segment = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.as_bytes());
        format!("header.{segment}.signature")
    }

    #[test]
    fn jwt_claims_reads_subject_and_expiry() {
        let token = jwt(r#"{"sub":"account-a","exp":4102444800,"iss":"https://regional-domain"}"#);
        let claims = jwt_claims(&token).expect("decodable claims");
        assert_eq!(claims["sub"], "account-a");
        assert_eq!(jwt_exp_unix_ms(&token), Some(4_102_444_800_000));
    }

    #[test]
    fn jwt_claims_rejects_garbage() {
        assert!(jwt_claims("not-a-jwt").is_none());
        assert!(jwt_claims("a.@@@.c").is_none());
        assert!(jwt_claims("a.b").is_none());
    }

    #[test]
    fn normalize_epoch_ms_handles_seconds_millis_and_strings() {
        assert_eq!(
            normalize_epoch_ms(Some(&Value::from(4_102_444_800i64))),
            Some(4_102_444_800_000)
        );
        assert_eq!(
            normalize_epoch_ms(Some(&Value::from(4_102_444_800_000i64))),
            Some(4_102_444_800_000)
        );
        assert_eq!(
            normalize_epoch_ms(Some(&Value::from("4102444800"))),
            Some(4_102_444_800_000)
        );
        assert_eq!(normalize_epoch_ms(Some(&Value::from("junk"))), None);
        assert_eq!(normalize_epoch_ms(Some(&Value::from(0))), None);
        assert_eq!(normalize_epoch_ms(None), None);
    }

    #[test]
    fn authorization_url_requires_plain_https() {
        assert!(validate_authorization_url("https://copilot.tencent.com/login?x=1").is_ok());
        assert!(validate_authorization_url("http://copilot.tencent.com/login").is_err());
        assert!(validate_authorization_url("https://user:pass@copilot.tencent.com/login").is_err());
        assert!(validate_authorization_url("https://user@copilot.tencent.com/login").is_err());
        assert!(validate_authorization_url("not a url").is_err());
    }

    #[test]
    fn header_values_reject_injection() {
        assert!(check_header_safe("uid", "account-a").is_ok());
        assert!(check_header_safe("uid", "a\r\nX-Injected: 1").is_err());
        assert!(check_header_safe("uid", "a b").is_err());
        assert!(check_header_safe("uid", "").is_err());
    }

    #[test]
    fn region_contradiction_only_on_known_opposite_markers() {
        assert!(contradicts_region(
            Region::Cn,
            "https://www.workbuddy.ai",
            None
        ));
        assert!(contradicts_region(
            Region::Intl,
            "",
            Some("copilot.tencent.com")
        ));
        assert!(contradicts_region(
            Region::Intl,
            "https://login.codebuddy.cn",
            None
        ));
        // 未知 issuer/domain 只是元数据，不构成区域矛盾。
        assert!(!contradicts_region(
            Region::Cn,
            "https://regional-domain",
            None
        ));
        assert!(!contradicts_region(
            Region::Intl,
            "https://regional-domain",
            None
        ));
    }

    #[test]
    fn rejects_malformed_auth_codes_without_echoing_server_text() {
        let malformed = serde_json::json!({"code":"error", "msg":"secret-token"});
        assert!(reject_nonzero_code(&malformed).is_err());
        let rejected = serde_json::json!({"code":123, "msg":"secret-token"});
        assert!(
            !reject_nonzero_code(&rejected)
                .unwrap_err()
                .message
                .contains("secret-token")
        );
    }
}
