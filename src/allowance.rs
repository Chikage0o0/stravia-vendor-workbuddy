use rust_decimal::Decimal;
use serde_json::{Value, json};
use stravia_vendor_common::common;
use stravia_vendor_sdk::{
    AllowanceAmount, AllowanceItem, AllowanceResponse, ErrorKind, GuestHost, HttpRequest,
    PluginError, ProviderSnapshot, read_http_body,
};

use crate::{Region, auth};

const RESOURCE_PATH: &str = "/v2/billing/meter/get-user-resource";
const PAGE_SIZE: usize = 100;
const MAX_PAGES: usize = 100;
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

pub(crate) fn execute(
    host: &GuestHost,
    provider: &ProviderSnapshot,
    region: Region,
) -> Result<AllowanceResponse, PluginError> {
    let mut totals = Credits::default();
    let begin = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    // 国内计费域与聊天域不同；地址不从令牌、响应或私有状态获取。
    let url = format!("{}{RESOURCE_PATH}", region.website());
    for page in 1..=MAX_PAGES {
        let body = serde_json::to_vec(&json!({
            "PageNumber": page,
            "PageSize": PAGE_SIZE,
            "ProductCode": "p_tcaca",
            "Status": [0, 3],
            "PackageEndTimeRangeBegin": begin,
            "PackageEndTimeRangeEnd": "2036-01-01 00:00:00"
        }))
        .map_err(|_| malformed("could not encode allowance request"))?;
        let response = host.http_start(HttpRequest {
            method: "POST".into(),
            url: url.clone(),
            headers: auth::billing_headers(provider, region)?,
            body,
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
        let rows = accounts(&value)?;
        for row in rows {
            totals.include(package(row)?)?;
        }
        if rows.len() < PAGE_SIZE {
            return Ok(AllowanceResponse {
                allowances: vec![totals.into_allowance(region)],
                models: Vec::new(),
                plan_label: None,
            });
        }
    }
    Err(common::plugin_error(
        ErrorKind::ResourceExhausted,
        "WorkBuddy allowance pagination exceeded the guest limit",
    ))
}

fn accounts(value: &Value) -> Result<&[Value], PluginError> {
    if value
        .get("code")
        .is_some_and(|code| code.as_i64() != Some(0))
        || value
            .pointer("/data/Response/Error")
            .is_some_and(|error| !error.is_null())
        || value
            .pointer("/Response/Error")
            .is_some_and(|error| !error.is_null())
    {
        return Err(malformed(
            "WorkBuddy billing service rejected the allowance query",
        ));
    }
    value
        .pointer("/data/Response/Data/Accounts")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| malformed("WorkBuddy allowance response has no Accounts array"))
}

struct Credits {
    remaining: Decimal,
    used: Option<Decimal>,
    total: Option<Decimal>,
}

impl Default for Credits {
    fn default() -> Self {
        // 只有成功读取完所有页面后才输出；明确的空资源列表得到零积分。
        Self {
            remaining: Decimal::ZERO,
            used: Some(Decimal::ZERO),
            total: Some(Decimal::ZERO),
        }
    }
}

impl Credits {
    fn include(&mut self, package: Self) -> Result<(), PluginError> {
        self.remaining = checked(self.remaining.checked_add(package.remaining))?;
        self.used = sum_optional(self.used, package.used)?;
        self.total = sum_optional(self.total, package.total)?;
        Ok(())
    }

    fn into_allowance(self, region: Region) -> AllowanceItem {
        let total = self
            .total
            .map(|value| value.normalize().to_string())
            .unwrap_or_else(|| "—".into());
        // 宿主遇到 limit 或 used_percent 会把剩余列渲染为百分比；
        // 已用/剩余列保留实际积分值，总量放在标题。多包无统一重置时间。
        let used_percent = self
            .total
            .filter(|limit| *limit > Decimal::ZERO)
            .map(|limit| {
                let used = self
                    .used
                    .unwrap_or_else(|| (limit - self.remaining).max(Decimal::ZERO));
                (used / limit * Decimal::ONE_HUNDRED)
                    .round_dp(2)
                    .clamp(Decimal::ZERO, Decimal::ONE_HUNDRED)
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
            used: self.used.map(credit),
            remaining: Some(credit(self.remaining)),
            limit: self.total.map(credit),
            used_percent,
            window_seconds: None,
            resets_at_unix_ms: None,
            condition: None,
        }
    }
}

fn package(row: &Value) -> Result<Credits, PluginError> {
    let cycle_limit = amount(row, "CycleCapacitySize")?;
    let cyclic = cycle_limit.is_some_and(|size| size > Decimal::ZERO);
    let (reported_used, remaining, mut total) = if cyclic {
        (
            amount(row, "CycleCapacityUsed")?,
            amount(row, "CycleCapacityRemain")?,
            cycle_limit,
        )
    } else {
        (
            amount(row, "CapacityUsed")?,
            amount(row, "CapacityRemain")?,
            amount(row, "CapacitySize")?,
        )
    };
    let mut remaining = remaining
        .ok_or_else(|| malformed("WorkBuddy resource package is missing its remaining capacity"))?;
    let used = match total {
        Some(size) => {
            let derived = checked(size.checked_sub(remaining))?.max(Decimal::ZERO);
            // 周期计数可能不同步：较新的已用量优先；
            // 较小或缺失的已用量则由总量减剩余量计算，避免漏掉小数用量。
            if cyclic && reported_used.is_some_and(|used| used > derived) {
                let used = reported_used.expect("checked above");
                remaining = checked(size.checked_sub(used))?.max(Decimal::ZERO);
                Some(used)
            } else {
                Some(derived)
            }
        }
        None => {
            total = reported_used
                .map(|used| checked(remaining.checked_add(used)))
                .transpose()?;
            reported_used
        }
    };
    Ok(Credits {
        remaining,
        used,
        total,
    })
}

fn amount(row: &Value, field: &str) -> Result<Option<Decimal>, PluginError> {
    let Some(value) = row.get(field).filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let text = match value {
        Value::Number(number) => number.to_string(),
        Value::String(text) => text.trim().to_owned(),
        _ => return Err(malformed("invalid WorkBuddy capacity value")),
    };
    let parsed = if let Some((base, _)) = text.split_once(['e', 'E']) {
        // from_scientific 的尾数解析允许舍入，先做精确检查。
        Decimal::from_str_exact(base).and_then(|_| Decimal::from_scientific(&text))
    } else {
        Decimal::from_str_exact(&text)
    };
    parsed
        .map(Some)
        .map_err(|_| malformed("invalid or out-of-range WorkBuddy capacity value"))
}

fn sum_optional(
    left: Option<Decimal>,
    right: Option<Decimal>,
) -> Result<Option<Decimal>, PluginError> {
    match (left, right) {
        (Some(left), Some(right)) => checked(left.checked_add(right)).map(Some),
        // 不能把某包未知的计数当成零，再把部分合计标为账号总量。
        _ => Ok(None),
    }
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
    use super::*;

    #[test]
    fn merges_cycle_and_reward_credits_without_losing_fractional_usage() {
        let mut totals = Credits::default();
        for row in [
            json!({"CycleCapacitySize":500,"CycleCapacityRemain":"334.16","CycleCapacityUsed":165,"CapacitySize":9999,"CapacityRemain":9999}),
            json!({"CapacitySize":4481,"CapacityRemain":4481}),
        ] {
            totals.include(package(&row).unwrap()).unwrap();
        }
        assert_eq!(totals.total.unwrap().to_string(), "4981");
        let result = totals.into_allowance(Region::Cn);
        assert!(result.label.contains("4981"));
        assert_eq!(result.remaining.unwrap().value, "4815.16");
        assert_eq!(result.used.unwrap().value, "165.84");
        assert_eq!(result.limit.unwrap().value, "4981");
        assert_eq!(result.used_percent.unwrap(), "3.33");
    }

    #[test]
    fn fractional_and_scientific_credits_sum_in_decimal() {
        let mut totals = Credits::default();
        for raw in [
            r#"{"CapacitySize":1,"CapacityRemain":0.1234567890123456789}"#,
            r#"{"CapacitySize":"2e-1","CapacityRemain":"1e-1"}"#,
        ] {
            totals
                .include(package(&serde_json::from_str(raw).unwrap()).unwrap())
                .unwrap();
        }
        let result = totals.into_allowance(Region::Intl);
        assert_eq!(result.remaining.unwrap().value, "0.2234567890123456789");
        assert_eq!(result.used.unwrap().value, "0.9765432109876543211");
    }

    #[test]
    fn zero_cycle_capacity_uses_resource_package_balance() {
        let row = json!({"PackageName":"Top-up","CycleCapacitySize":0,"CycleCapacityRemain":500,"CapacityRemain":"0","CapacityUsed":"20.5","CapacitySize":"20.5"});
        let result = package(&row).unwrap().into_allowance(Region::Cn);
        assert_eq!(result.remaining.unwrap().value, "0");
        assert_eq!(result.used.unwrap().value, "20.5");
    }

    #[test]
    fn newer_cycle_usage_corrects_stale_remaining() {
        let result = package(
            &json!({"CycleCapacitySize":100,"CycleCapacityRemain":80,"CycleCapacityUsed":53}),
        )
        .unwrap()
        .into_allowance(Region::Cn);
        assert_eq!(result.remaining.unwrap().value, "47");
        assert_eq!(result.used.unwrap().value, "53");
    }

    #[test]
    fn missing_counters_do_not_produce_partial_account_totals() {
        let mut totals = Credits::default();
        totals
            .include(package(&json!({"CapacityRemain":7})).unwrap())
            .unwrap();
        totals
            .include(package(&json!({"CapacityRemain":20,"CapacitySize":25})).unwrap())
            .unwrap();
        assert!(totals.total.is_none());
        let result = totals.into_allowance(Region::Cn);
        assert_eq!(result.remaining.unwrap().value, "27");
        assert!(result.used.is_none());
        assert!(result.limit.is_none());
        assert!(result.used_percent.is_none());
        let reconstructed = package(&json!({"CapacityRemain":"1.2","CapacityUsed":"0.3"})).unwrap();
        assert_eq!(reconstructed.total.unwrap().to_string(), "1.5");
    }

    #[test]
    fn overflowing_totals_fail_instead_of_publishing_a_partial_balance() {
        let mut totals = Credits::default();
        let row = json!({"CapacityRemain":Decimal::MAX.to_string()});
        totals.include(package(&row).unwrap()).unwrap();
        assert!(totals.include(package(&row).unwrap()).is_err());
    }

    #[test]
    fn malformed_and_failed_queries_are_not_empty_balances() {
        for payload in [
            json!({}),
            json!({"code":123,"msg":"secret"}),
            json!({"data":{"Response":{"Error":{"Code":"Denied"},"Data":{"Accounts":[]}}}}),
        ] {
            assert!(accounts(&payload).is_err());
        }
        for row in [
            json!({}),
            json!({"CapacityRemain":"NaN"}),
            json!({"CapacityRemain":true}),
            json!({"CapacityRemain":"1e1000"}),
            json!({"CapacityRemain":"0.00000000000000000000000000001"}),
        ] {
            assert!(package(&row).is_err());
        }
        assert!(
            accounts(&json!({"data":{"Response":{"Data":{"Accounts":[]}}}}))
                .unwrap()
                .is_empty()
        );
    }
}
