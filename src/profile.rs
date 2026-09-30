use std::collections::BTreeSet;

use semver::Version;
use stravia_vendor_sdk::{
    AuthDescriptor, AuthFlow, CANONICAL_FORMAT_VERSION, Capability, ChannelDescriptor, ConfigField,
    ConfigFieldKind, DataCompatibility, NetworkDeclaration, OriginDeclaration, ProviderDescriptor,
    VendorDescriptor, VendorKind,
};

use crate::{Region, VENDOR_ID, messages};

pub(crate) fn descriptor() -> VendorDescriptor {
    let capabilities = BTreeSet::from([
        Capability::Infer,
        Capability::AuthOauth,
        Capability::ModelDiscovery,
        Capability::Allowance,
        Capability::ConfigValidation,
    ]);
    let channels = [Region::Cn, Region::Intl]
        .into_iter()
        .map(|region| ChannelDescriptor {
            id: region.id().into(),
            name: match region {
                Region::Cn => messages::channel_cn(),
                Region::Intl => messages::channel_intl(),
            },
            description: Some(match region {
                Region::Cn => messages::channel_cn_description(),
                Region::Intl => messages::channel_intl_description(),
            }),
            auth: Some(AuthDescriptor {
                flow: AuthFlow::DeviceCode,
                callback: None,
                manual_input: None,
            }),
            protocol: Some("openai-compatible".into()),
            protocols: Vec::new(),
            default_base_url: Some(region.origin().into()),
            default_models_source: None,
            consumes_catalog_models: false,
            capabilities: capabilities.clone(),
            model_capabilities: BTreeSet::new(),
            search_model_required: false,
        })
        .collect();
    VendorDescriptor {
        vendor_id: VENDOR_ID.into(),
        version: Version::parse(env!("CARGO_PKG_VERSION")).expect("valid Cargo version"),
        display_name: "WorkBuddy".into(),
        description: Some("WorkBuddy China and international browser-login provider".into()),
        authors: vec!["Chikage0o0 <chikage@939.me>".into()],
        canonical_format_version: CANONICAL_FORMAT_VERSION,
        kind: VendorKind::Dedicated,
        providers: vec![ProviderDescriptor {
            provider_id: VENDOR_ID.into(),
            catalog_id: None,
            display_name: "WorkBuddy".into(),
            description: Some("Unofficial WorkBuddy integration; account and model availability are controlled by the upstream service.".into()),
            channels,
            capabilities,
            website: Some("https://www.workbuddy.ai".into()),
            implementation: None,
            config_groups: Vec::new(),
            config_fields: vec![ConfigField {
                key: "auto_paid_on_rate_limit".into(),
                label: messages::auto_paid_on_rate_limit(),
                description: Some(messages::auto_paid_on_rate_limit_description()),
                kind: ConfigFieldKind::Bool,
                required: false,
                default_json: Some(false.into()),
                group: None,
                secret: false,
                min: None,
                max: None,
                max_length: None,
                pattern: None,
                visible_when: None,
            }, ConfigField {
                key: "model_ids".into(),
                label: messages::model_ids(),
                description: Some(messages::model_ids_description()),
                kind: ConfigFieldKind::String { multiline: true },
                required: false,
                default_json: None,
                group: None,
                secret: false,
                min: None,
                max: None,
                max_length: Some(16384),
                pattern: None,
                visible_when: None,
            }],
            // 国内额度属于独立计费域名；聊天与国际额度仍使用各自连接 origin。
            network: NetworkDeclaration {
                extra_origins: vec![OriginDeclaration {
                    scheme: "https".into(),
                    host: "www.codebuddy.cn".into(),
                    port: None,
                }],
                ..NetworkDeclaration::default()
            },
            data_compat: DataCompatibility::default(),
        }],
    }
}
