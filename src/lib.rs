mod allowance;
mod auth;
mod inference;
mod models;
mod profile;
mod state;
mod messages {
    include!(concat!(env!("OUT_DIR"), "/messages.rs"));
}

use stravia_runtime_contract::protocol::ir::AiRequest;
use stravia_vendor_common::common;
use stravia_vendor_sdk::{
    ErrorKind, GuestHost, Operation, OperationInput, OperationOutput, PluginError,
    ProviderSnapshot, VendorDescriptor, VendorGuest,
};

pub const VENDOR_ID: &str = "workbuddy";
pub(crate) const PROTOCOL: &str = "openai-compatible/chat-completions/v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Region {
    Cn,
    Intl,
}

impl Region {
    pub(crate) fn from_channel(channel: &str) -> Result<Self, PluginError> {
        match channel {
            "cn" => Ok(Self::Cn),
            "intl" => Ok(Self::Intl),
            _ => Err(common::unsupported("channel", VENDOR_ID, channel)),
        }
    }

    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Cn => "cn",
            Self::Intl => "intl",
        }
    }

    pub(crate) fn origin(self) -> &'static str {
        match self {
            Self::Cn => "https://copilot.tencent.com",
            Self::Intl => "https://www.workbuddy.ai",
        }
    }

    pub(crate) fn domain(self) -> &'static str {
        match self {
            Self::Cn => "copilot.tencent.com",
            Self::Intl => "www.workbuddy.ai",
        }
    }

    pub(crate) fn website(self) -> &'static str {
        match self {
            Self::Cn => "https://www.codebuddy.cn",
            Self::Intl => self.origin(),
        }
    }

    pub(crate) fn version(self) -> &'static str {
        match self {
            Self::Cn => "5.6.2",
            Self::Intl => "5.6.2",
        }
    }

    pub(crate) fn user_agent(self) -> &'static str {
        match self {
            Self::Cn => "WorkBuddy/5.6.2 WorkBuddy/5.6.2 CLI/2.147.0",
            Self::Intl => "WorkBuddy/5.6.2 WorkBuddy AI/5.6.2 CLI/5.6.2",
        }
    }
}

pub fn descriptor() -> VendorDescriptor {
    profile::descriptor()
}

fn admit(channel: &str, provider: &ProviderSnapshot) -> Result<Region, PluginError> {
    if provider.provider_id != VENDOR_ID || provider.channel != channel {
        return Err(common::plugin_error(
            ErrorKind::Invalid,
            "WorkBuddy provider/channel mismatch",
        ));
    }
    let region = Region::from_channel(channel)?;
    // 连接地址固定到所选区域，避免更换地址或 channel 后向其他区域发送旧凭据。
    if provider.base_url.trim_end_matches('/') != region.origin() {
        return Err(common::plugin_error(
            ErrorKind::Invalid,
            "WorkBuddy base URL does not match the selected region",
        ));
    }
    Ok(region)
}

pub struct WorkBuddy;

impl VendorGuest for WorkBuddy {
    fn descriptor() -> VendorDescriptor {
        descriptor()
    }

    fn select_protocol(
        operation: Operation,
        channel: &str,
        provider: &ProviderSnapshot,
        _request: &AiRequest,
    ) -> Result<String, PluginError> {
        admit(channel, provider)?;
        if operation != Operation::Infer {
            return Err(common::unsupported(operation.as_str(), VENDOR_ID, channel));
        }
        Ok(PROTOCOL.into())
    }

    fn execute(
        host: &GuestHost,
        operation: Operation,
        channel: &str,
        input: OperationInput,
    ) -> Result<OperationOutput, PluginError> {
        if operation != input.operation() {
            return Err(common::plugin_error(
                ErrorKind::Invalid,
                "operation/input mismatch",
            ));
        }
        let region = admit(channel, input.provider())?;
        match input {
            OperationInput::Infer { provider, request } => {
                inference::execute(host, &provider, region, request)
            }
            OperationInput::Auth { provider, request } => {
                auth::execute(host, &provider, region, request).map(OperationOutput::Auth)
            }
            OperationInput::Discover { provider, request } => {
                models::discover(host, &provider, region, request).map(OperationOutput::Discover)
            }
            OperationInput::Allowance {
                provider,
                request: _,
            } => allowance::execute(host, &provider, region).map(OperationOutput::Allowance),
            OperationInput::ConfigValidation {
                provider: _,
                request,
            } => Ok(OperationOutput::ConfigValidation(models::validate(
                &request.options,
            ))),
            _ => Err(common::unsupported(operation.as_str(), VENDOR_ID, channel)),
        }
    }
}

#[cfg(target_arch = "wasm32")]
stravia_vendor_sdk::export_vendor!(WorkBuddy);
