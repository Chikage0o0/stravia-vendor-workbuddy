//! WorkBuddy 图片操作：固定区域 API、单张 JSON 响应与受限的无凭据产物下载。

use std::collections::BTreeMap;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use stravia_runtime_contract::protocol::ir::Usage;
use stravia_vendor_common::common;
use stravia_vendor_sdk::{
    AiErrorKind, ErrorKind, GuestHost, HttpRequest, MediaArtifact, MediaImageAspectRatio,
    MediaImageRequest, MediaImageResolution, MediaImageResponse, PluginError, ProviderSnapshot,
    read_http_body,
};

use crate::{Region, auth, request_context::RequestContext};

pub(crate) const IMAGE_CDN_HOST_REGEX: &str = r"[a-z0-9-]+\.cos\.[a-z0-9-]+\.myqcloud\.com";
const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;
const MAX_ENCODED_BYTES: usize = MAX_IMAGE_BYTES.div_ceil(3) * 4;
const MAX_RESPONSE_BYTES: usize = MAX_ENCODED_BYTES + 2 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 256 * 1024;

pub(crate) fn generate(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    region: Region,
    request: MediaImageRequest,
) -> Result<MediaImageResponse, PluginError> {
    let model = provider
        .model_metadata
        .as_ref()
        .and_then(|metadata| metadata.selector.as_deref())
        .or(provider.model.as_deref())
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .ok_or_else(|| invalid("WorkBuddy image generation requires a model"))?;
    if !crate::models::validate(&provider.options).issues.is_empty() {
        return Err(invalid("invalid WorkBuddy model options"));
    }
    let edit = !request.references.is_empty();
    let body = build_request(model, request)?;
    let mut headers = auth::headers(provider, region)?;
    RequestContext::new(host, provider).apply(&mut headers);
    let body = serde_json::to_vec(&body)
        .map_err(|_| invalid("WorkBuddy image request could not be encoded"))?;
    host.emit_started()?;
    let response = host.http_start(HttpRequest {
        method: "POST".into(),
        url: format!(
            "{}/v2/images/{}",
            region.origin(),
            if edit { "edits" } else { "generations" }
        ),
        headers,
        body,
    })?;
    let status = response.status()?;
    let headers = response.headers()?;
    let body = read_http_body(
        &response,
        if (200..300).contains(&status) {
            MAX_RESPONSE_BYTES
        } else {
            MAX_ERROR_BYTES
        },
    )?;
    if !(200..300).contains(&status) {
        let mut error = common::upstream_error(status, &headers, &body);
        // 上游错误可能回显提示词或其他输入，不将原始消息写入宿主日志。
        error.message = format!("WorkBuddy image generation HTTP {status}");
        return Err(error);
    }
    let payload: Value = serde_json::from_slice(&body)
        .map_err(|_| upstream("WorkBuddy image response contains invalid JSON"))?;
    drop(body);
    let (mut artifact, revised_prompt, raw_usage) = parse_response(payload)?;
    if let Some(url) = artifact.upstream_ref.take() {
        let url = download_url(&url)?;
        // COS 签名 URL 不属于账号 API；绝不复用 Authorization、账号或会话标头。
        let response = host.http_start(HttpRequest {
            method: "GET".into(),
            url: url.into(),
            headers: vec![("Accept".into(), "image/*".into())],
            body: Vec::new(),
        })?;
        let status = response.status()?;
        if !(200..300).contains(&status) {
            return Err(upstream(&format!("WorkBuddy image download HTTP {status}")));
        }
        let headers = response.headers()?;
        let mime = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value.split(';').next().unwrap_or_default().trim())
            .filter(|mime| image_mime(mime))
            .ok_or_else(|| upstream("WorkBuddy image download has invalid MIME"))?;
        artifact.media_type = mime.to_owned();
        artifact.bytes = read_http_body(&response, MAX_IMAGE_BYTES)?;
        if artifact.bytes.is_empty() {
            return Err(upstream("WorkBuddy returned empty image bytes"));
        }
    }
    let usage = parse_usage(raw_usage.as_ref())?;
    if let Some(usage) = raw_usage {
        // credit 与图片计数不是 token；保留原始计量事实，不伪造 canonical token 用量。
        artifact.metadata.insert("workbuddy_usage".into(), usage);
    }
    Ok(MediaImageResponse {
        artifacts: vec![artifact],
        revised_prompt,
        usage,
    })
}

fn build_request(model: &str, request: MediaImageRequest) -> Result<Value, PluginError> {
    if request.prompt.trim().is_empty() {
        return Err(invalid("WorkBuddy image prompt must not be empty"));
    }
    if request.references.len() > 3 {
        return Err(invalid("WorkBuddy accepts at most three reference images"));
    }
    let size = image_size(request.aspect_ratio, request.resolution);
    let mut body = json!({"model":model, "prompt":request.prompt, "size":size});
    if model.starts_with("hunyuan-") {
        body["n"] = json!(1);
    } else if !model.starts_with("gemini-") {
        body["n"] = json!(1);
        body["response_format"] = json!("b64_json");
    }
    if !request.references.is_empty() {
        let mut images = Vec::with_capacity(request.references.len());
        for reference in request.references {
            if reference.bytes.is_empty()
                || reference.bytes.len() > MAX_IMAGE_BYTES
                || !image_mime(&reference.media_type)
            {
                return Err(invalid(
                    "WorkBuddy references require non-empty image bytes of at most 32 MiB",
                ));
            }
            let mut data = String::with_capacity(
                reference.media_type.len() + 13 + reference.bytes.len().div_ceil(3) * 4,
            );
            data.push_str("data:");
            data.push_str(&reference.media_type);
            data.push_str(";base64,");
            STANDARD.encode_string(&reference.bytes, &mut data);
            images.push(Value::String(data));
        }
        body["image"] = Value::Array(images);
    }
    Ok(body)
}

fn image_size(
    ratio: Option<MediaImageAspectRatio>,
    resolution: Option<MediaImageResolution>,
) -> String {
    let (width, height) = match ratio.unwrap_or(MediaImageAspectRatio::Square) {
        MediaImageAspectRatio::Square => (1024, 1024),
        MediaImageAspectRatio::Portrait => (768, 1024),
        MediaImageAspectRatio::Landscape => (1024, 768),
        MediaImageAspectRatio::Tall => (576, 1024),
        MediaImageAspectRatio::Wide => (1024, 576),
    };
    let scale = match resolution.unwrap_or(MediaImageResolution::OneK) {
        MediaImageResolution::OneK => 1,
        MediaImageResolution::TwoK => 2,
        MediaImageResolution::FourK => 4,
    };
    format!("{}x{}", width * scale, height * scale)
}

fn parse_response(
    mut payload: Value,
) -> Result<(MediaArtifact, Option<String>, Option<Value>), PluginError> {
    match payload.get("code").and_then(Value::as_i64) {
        Some(0) => {}
        Some(code) => {
            return Err(upstream(&format!(
                "WorkBuddy image generation returned error code {code}"
            )));
        }
        None => {
            return Err(upstream(
                "WorkBuddy image response has no valid status code",
            ));
        }
    }
    let data = payload
        .get_mut("data")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| upstream("WorkBuddy image response has no data object"))?;
    let mut items = data
        .remove("data")
        .and_then(|value| match value {
            Value::Array(items) => Some(items),
            _ => None,
        })
        .filter(|items| items.len() == 1)
        .ok_or_else(|| upstream("WorkBuddy must return exactly one image"))?;
    let mut item = items
        .pop()
        .and_then(|value| match value {
            Value::Object(item) => Some(item),
            _ => None,
        })
        .ok_or_else(|| upstream("WorkBuddy returned an invalid image entry"))?;
    let revised_prompt = match item.remove("revised_prompt") {
        None | Some(Value::Null) => None,
        Some(Value::String(prompt)) => Some(prompt),
        Some(_) => return Err(upstream("WorkBuddy returned an invalid revised prompt")),
    };
    let mut artifact = MediaArtifact {
        media_type: "image/png".into(),
        bytes: Vec::new(),
        upstream_ref: None,
        metadata: BTreeMap::new(),
    };
    if let Some(encoded) = item.get("b64_json") {
        let encoded = encoded
            .as_str()
            .filter(|value| {
                !value.is_empty() && value.len() % 4 == 0 && value.len() <= MAX_ENCODED_BYTES
            })
            .ok_or_else(|| upstream("WorkBuddy image has invalid base64 size"))?;
        let padding = encoded
            .bytes()
            .rev()
            .take_while(|byte| *byte == b'=')
            .count();
        if padding > 2 || encoded.len() / 4 * 3 - padding > MAX_IMAGE_BYTES {
            return Err(upstream("WorkBuddy image exceeds the decoded size limit"));
        }
        artifact.bytes = STANDARD
            .decode(encoded)
            .map_err(|_| upstream("WorkBuddy image contains invalid base64"))?;
        if artifact.bytes.is_empty() || artifact.bytes.len() > MAX_IMAGE_BYTES {
            return Err(upstream("WorkBuddy image exceeds the decoded size limit"));
        }
    } else {
        artifact.upstream_ref = Some(
            item.remove("url")
                .and_then(|value| match value {
                    Value::String(url) if !url.is_empty() => Some(url),
                    _ => None,
                })
                .ok_or_else(|| upstream("WorkBuddy image has neither bytes nor a URL"))?,
        );
    }
    Ok((artifact, revised_prompt, data.remove("usage")))
}

fn download_url(value: &str) -> Result<url::Url, PluginError> {
    let url =
        url::Url::parse(value).map_err(|_| upstream("WorkBuddy returned an invalid image URL"))?;
    if url.scheme() != "https"
        || !matches!(url.port(), None | Some(443))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(upstream(
            "WorkBuddy image URL requires HTTPS on port 443 without credentials or a fragment",
        ));
    }
    // 主机准入由宿主冻结的权限策略执行，不在插件中重复解析正则或从响应扩权。
    Ok(url)
}

fn parse_usage(value: Option<&Value>) -> Result<Option<Usage>, PluginError> {
    let Some(value) = value else { return Ok(None) };
    let object = value
        .as_object()
        .ok_or_else(|| upstream("WorkBuddy image usage is not an object"))?;
    let count = |key| -> Result<Option<u32>, PluginError> {
        object
            .get(key)
            .map(|value| {
                value
                    .as_u64()
                    .and_then(|value| u32::try_from(value).ok())
                    .ok_or_else(|| upstream("WorkBuddy image token usage is invalid"))
            })
            .transpose()
    };
    let input = count("input_tokens")?;
    let output = count("output_tokens")?;
    let total = count("total_tokens")?;
    if input.is_none() && output.is_none() && total.is_none() {
        return Ok(None);
    }
    Ok(Some(Usage {
        prompt_tokens: input.unwrap_or_default(),
        completion_tokens: output.unwrap_or_default(),
        total_tokens: total.unwrap_or_default(),
        required_components_known: input.is_some() && output.is_some() && total.is_some(),
        ..Default::default()
    }))
}

// 像素格式与实际内容由 Core 解码核验，不在插件重复实现图片解析器。
fn image_mime(mime: &str) -> bool {
    mime.get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("image/"))
        && mime.len() > 6
        && mime
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'+' | b'.'))
}

fn invalid(message: &str) -> PluginError {
    common::plugin_error(ErrorKind::Invalid, message)
}

fn upstream(message: &str) -> PluginError {
    common::model_error(AiErrorKind::ServerError, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use stravia_vendor_sdk::MediaReference;

    fn request() -> MediaImageRequest {
        MediaImageRequest {
            prompt: "蓝色圆形".into(),
            references: Vec::new(),
            aspect_ratio: None,
            resolution: None,
        }
    }

    #[test]
    fn references_preserve_bytes_and_reject_invalid_inputs() {
        let mut input = request();
        input.references.push(MediaReference {
            media_type: "image/png".into(),
            bytes: vec![0, 1, 2, 255],
        });
        let body = build_request("gpt-image", input).unwrap();
        assert_eq!(body["image"], json!(["data:image/png;base64,AAEC/w=="]));
        for (mime, bytes) in [("text/plain", vec![1]), ("image/png", vec![])] {
            let mut input = request();
            input.references.push(MediaReference {
                media_type: mime.into(),
                bytes,
            });
            assert!(matches!(
                build_request("gpt-image", input).unwrap_err().kind,
                ErrorKind::Invalid
            ));
        }
        let mut input = request();
        input.prompt = " \n".into();
        assert!(build_request("hunyuan-image", input).is_err());
        let mut input = request();
        input.references = (0..4)
            .map(|_| MediaReference {
                media_type: "image/png".into(),
                bytes: vec![1],
            })
            .collect();
        assert!(build_request("gpt-image", input).is_err());
    }

    #[test]
    fn ratio_and_resolution_preserve_geometry() {
        for (ratio, expected) in [
            (MediaImageAspectRatio::Square, "4096x4096"),
            (MediaImageAspectRatio::Portrait, "3072x4096"),
            (MediaImageAspectRatio::Landscape, "4096x3072"),
            (MediaImageAspectRatio::Tall, "2304x4096"),
            (MediaImageAspectRatio::Wide, "4096x2304"),
        ] {
            assert_eq!(
                image_size(Some(ratio), Some(MediaImageResolution::FourK)),
                expected
            );
        }
    }

    #[test]
    fn image_urls_require_https_without_credentials_or_fragments() {
        for value in [
            "http://images-1250000001.cos.ap-shanghai.myqcloud.com/image.png",
            "https://www.workbuddy.cn:444/image.png",
            "https://user:secret@www.workbuddy.cn/image.png",
            "https://www.workbuddy.cn/image.png#fragment",
        ] {
            assert!(download_url(value).is_err(), "{value}");
        }
        for url in [
            "https://images-1250000001.cos.ap-shanghai.myqcloud.com/image.png?signature=synthetic",
            "https://images-1250000002.cos.ap-singapore.myqcloud.com/image.png?signature=synthetic",
        ] {
            assert_eq!(download_url(url).unwrap().as_str(), url);
        }
    }

    #[test]
    fn failed_empty_multiple_and_corrupt_images_are_rejected() {
        for payload in [
            json!({"code":14401, "data":{"data":[{"b64_json":"AA=="}]}}),
            json!({"data":{"data":[{"b64_json":"AA=="}]}}),
            json!({"code":0, "data":{"data":[]}}),
            json!({"code":0, "data":{"data":[{}, {}]}}),
            json!({"code":0, "data":{"data":[{}]}}),
            json!({"code":0, "data":{"data":[{"b64_json":""}]}}),
            json!({"code":0, "data":{"data":[{"b64_json":"!!!!"}]}}),
            json!({"code":0, "data":{"data":[{"b64_json":"A==="}]}}),
        ] {
            assert!(parse_response(payload).is_err());
        }
    }

    #[test]
    fn absent_token_usage_is_not_reported_as_zero_and_credit_is_not_a_token() {
        assert!(parse_usage(None).unwrap().is_none());
        assert!(
            parse_usage(Some(&json!({"credit":5.71})))
                .unwrap()
                .is_none()
        );
        let usage = parse_usage(Some(&json!({
            "input_tokens":346, "output_tokens":464, "total_tokens":810, "credit":5.71
        })))
        .unwrap()
        .unwrap();
        assert_eq!(usage.prompt_tokens, 346);
        assert_eq!(usage.completion_tokens, 464);
        assert_eq!(usage.total_tokens, 810);
        assert!(usage.required_components_known);
        assert!(parse_usage(Some(&json!({"input_tokens":-1}))).is_err());
        assert!(
            !parse_usage(Some(&json!({"input_tokens":4})))
                .unwrap()
                .unwrap()
                .required_components_known
        );
    }
}
