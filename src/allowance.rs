use rust_decimal::Decimal;
use serde_json::Value;
use stravia_vendor_common::{account::first_account_label, common};
use stravia_vendor_sdk::{
    AllowanceAmount, AllowanceItem, AllowanceResponse, ErrorKind, GuestHost, HttpRequest, LogLevel,
    PluginError, ProviderSnapshot, read_http_body,
};

use crate::{Region, auth};

const SUMMARY_PATH: &str = "/billing/meter/get-user-resource-summary";
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

pub(crate) fn execute(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    region: Region,
) -> Result<AllowanceResponse, PluginError> {
    let response = host.http_start(HttpRequest {
        method: "POST".into(),
        url: format!("{}{SUMMARY_PATH}", region.origin()),
        headers: auth::billing_headers(provider, region)?,
        body: b"{}".to_vec(),
    })?;
    let status = response.status()?;
    let headers = response.headers()?;
    let bytes = read_http_body(&response, MAX_BODY_BYTES)?;
    if !(200..300).contains(&status) {
        let mut error = common::upstream_error(status, &headers, &bytes);
        error.message = format!("WorkBuddy allowance HTTP {status}");
        return Err(error);
    }
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|_| malformed("invalid allowance JSON"))?;
    let mut totals = Credits::default();
    for row in packages(&value)? {
        totals.include(package(row)?)?;
    }
    // 资料只是附加展示；读取失败不能抹掉已经成功取得的积分。
    let account_label = load_account_label(host, provider, region).unwrap_or_else(|_| {
        host.log(LogLevel::Warn, "WorkBuddy account profile is unavailable");
        None
    });
    Ok(AllowanceResponse {
        allowances: vec![totals.into_allowance(region)],
        models: Vec::new(),
        // 资源包名不是已确认的订阅等级，不据此推断套餐。
        plan_label: None,
        account_label,
    })
}

fn load_account_label(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    region: Region,
) -> Result<Option<String>, PluginError> {
    let path = match region {
        Region::Cn => "/v2/plugin/account",
        Region::Intl => "/v2/plugin/accounts",
    };
    let response = host.http_start(HttpRequest {
        method: "GET".into(),
        url: format!("{}{path}", region.origin()),
        headers: auth::headers(provider, region)?,
        body: Vec::new(),
    })?;
    let status = response.status()?;
    if !(200..300).contains(&status) {
        return Err(malformed("WorkBuddy account profile HTTP request failed"));
    }
    let bytes = read_http_body(&response, MAX_BODY_BYTES)?;
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|_| malformed("invalid account profile JSON"))?;
    if value.get("code").and_then(Value::as_i64) != Some(0) {
        return Err(malformed(
            "WorkBuddy account service rejected the profile query",
        ));
    }
    Ok(account_label(&value, provider, region))
}

fn account_label(value: &Value, provider: &ProviderSnapshot, region: Region) -> Option<String> {
    let uid = provider.credentials.get("uid")?.as_str()?.trim();
    let enterprise_id = provider
        .credentials
        .get("enterprise_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty());
    let data = value.get("data")?;
    let accounts = match region {
        Region::Cn => std::slice::from_ref(data),
        Region::Intl => data.get("accounts")?.as_array()?.as_slice(),
    };
    let mut matching = accounts.iter().filter(|account| {
        let account_enterprise_id = match account.get("enterpriseId") {
            None | Some(Value::Null) => None,
            Some(Value::String(id)) => Some(id.as_str()).filter(|id| !id.is_empty()),
            _ => return false,
        };
        account.get("uid").and_then(Value::as_str) == Some(uid)
            && account_enterprise_id == enterprise_id
    });
    let account = matching.next()?;
    // 列表顺序不是账号身份；歧义时宁可不显示，也不选其他账号或企业。
    if matching.next().is_some() {
        return None;
    }
    first_account_label(["email", "nickname"].map(|key| account.get(key).and_then(Value::as_str)))
}

fn packages(value: &Value) -> Result<&[Value], PluginError> {
    if value.get("code").and_then(Value::as_i64) != Some(0) {
        return Err(malformed(
            "WorkBuddy billing service rejected the allowance query",
        ));
    }
    value
        .pointer("/data/Packages")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| malformed("WorkBuddy allowance summary has no Packages array"))
}

#[derive(Default)]
struct Credits {
    remaining: Decimal,
    used: Decimal,
    total: Decimal,
}

impl Credits {
    fn include(&mut self, package: Self) -> Result<(), PluginError> {
        self.remaining = checked(self.remaining.checked_add(package.remaining))?;
        self.used = checked(self.used.checked_add(package.used))?;
        self.total = checked(self.total.checked_add(package.total))?;
        Ok(())
    }

    fn into_allowance(self, region: Region) -> AllowanceItem {
        let total = self.total.normalize().to_string();
        // 汇总接口直接给出本周期的精确计数，不再从资源包余额反推或跨周期相加。
        let used_percent = (self.total > Decimal::ZERO).then(|| {
            (self.used.clamp(Decimal::ZERO, self.total) / self.total * Decimal::ONE_HUNDRED)
                .round_dp(2)
                .normalize()
                .to_string()
        });
        AllowanceItem {
            key: "workbuddy_credits_total".into(),
            label: match region {
                Region::Cn => format!("总积分 {total}"),
                Region::Intl => format!("Total credits {total}"),
            },
            kind: "balance".into(),
            used: Some(credit(self.used)),
            remaining: Some(credit(self.remaining)),
            limit: Some(credit(self.total)),
            used_percent,
            window_seconds: None,
            resets_at_unix_ms: None,
            condition: None,
        }
    }
}

fn package(row: &Value) -> Result<Credits, PluginError> {
    Ok(Credits {
        remaining: amount(row, "CycleRemainCapacity")?,
        used: amount(row, "CycleUsedCapacity")?,
        total: amount(row, "CycleTotalCapacity")?,
    })
}

fn amount(row: &Value, field: &str) -> Result<Decimal, PluginError> {
    let number_text;
    let text = match row.get(field) {
        Some(Value::Number(number)) => {
            number_text = number.to_string();
            number_text.as_str()
        }
        Some(Value::String(text)) => text.trim(),
        _ => return Err(malformed("missing or invalid WorkBuddy capacity value")),
    };
    let parsed = if let Some((base, _)) = text.split_once(['e', 'E']) {
        // from_scientific 的尾数解析允许舍入，先做精确检查。
        Decimal::from_str_exact(base).and_then(|_| Decimal::from_scientific(text))
    } else {
        Decimal::from_str_exact(text)
    };
    parsed.map_err(|_| malformed("invalid or out-of-range WorkBuddy capacity value"))
}

fn checked(value: Option<Decimal>) -> Result<Decimal, PluginError> {
    value.ok_or_else(|| malformed("WorkBuddy credit arithmetic exceeded decimal range"))
}

fn credit(value: Decimal) -> AllowanceAmount {
    AllowanceAmount {
        value: value.normalize().to_string(),
        unit: "credits".into(),
        currency: None,
    }
}

fn malformed(message: &str) -> PluginError {
    common::plugin_error(ErrorKind::upstream_unknown(), message)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn summary_packages_preserve_fractional_account_usage() {
        let mut totals = Credits::default();
        for row in [
            json!({"CycleTotalCapacity":500,"CycleRemainCapacity":"334.16","CycleUsedCapacity":"165.84"}),
            json!({"CycleTotalCapacity":4481,"CycleRemainCapacity":4481,"CycleUsedCapacity":0}),
        ] {
            totals.include(package(&row).unwrap()).unwrap();
        }
        let result = totals.into_allowance(Region::Cn);
        assert_eq!(result.remaining.unwrap().value, "4815.16");
        assert_eq!(result.used.unwrap().value, "165.84");
        assert_eq!(result.limit.unwrap().value, "4981");
        assert_eq!(result.used_percent.unwrap(), "3.33");
    }

    #[test]
    fn fractional_and_scientific_credits_sum_in_decimal() {
        let mut totals = Credits::default();
        for raw in [
            r#"{"CycleTotalCapacity":1,"CycleRemainCapacity":0.1234567890123456789,"CycleUsedCapacity":"0.8765432109876543211"}"#,
            r#"{"CycleTotalCapacity":"2e-1","CycleRemainCapacity":"1e-1","CycleUsedCapacity":"1e-1"}"#,
        ] {
            totals
                .include(package(&serde_json::from_str(raw).unwrap()).unwrap())
                .unwrap();
        }
        let result = totals.into_allowance(Region::Intl);
        assert_eq!(result.remaining.unwrap().value, "0.2234567890123456789");
        assert_eq!(result.used.unwrap().value, "0.9765432109876543211");
        assert_eq!(result.limit.unwrap().value, "1.2");
    }

    #[test]
    fn overflowing_totals_fail_instead_of_publishing_a_partial_balance() {
        let mut totals = Credits::default();
        let row = json!({
            "CycleTotalCapacity":Decimal::MAX.to_string(),
            "CycleRemainCapacity":Decimal::MAX.to_string(),"CycleUsedCapacity":0
        });
        totals.include(package(&row).unwrap()).unwrap();
        assert!(totals.include(package(&row).unwrap()).is_err());
    }

    #[test]
    fn malformed_and_failed_queries_are_not_empty_balances() {
        for payload in [
            json!({}),
            json!({"code":123,"msg":"secret"}),
            json!({"code":0,"data":{}}),
            json!({"code":0,"data":{"Packages":null}}),
        ] {
            assert!(packages(&payload).is_err());
        }
        for invalid in [
            Value::Null,
            json!("NaN"),
            json!(true),
            json!("1e1000"),
            json!("0.00000000000000000000000000001"),
        ] {
            assert!(
                package(&json!({
                    "CycleTotalCapacity":10,"CycleRemainCapacity":invalid,"CycleUsedCapacity":0
                }))
                .is_err()
            );
        }
        assert!(package(&json!({"CycleRemainCapacity":7})).is_err());
        assert!(
            packages(&json!({"code":0,"data":{"Packages":[]}}))
                .unwrap()
                .is_empty()
        );
    }
}
