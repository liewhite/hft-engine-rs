//! IBKR symbol ↔ conid 解析
//!
//! 通过 REST API 将 symbol 列表解析为 conid 映射

use super::auth::IbkrAuth;
use reqwest::Client;
use std::collections::HashMap;

/// 通过 REST API 批量解析 symbols 到 conid 映射
///
/// GET /trsrv/stocks?symbols=AAPL,NVDA,...
/// 过滤 isUS=true 的合约，取第一个 conid
pub async fn resolve_conids(
    http: &Client,
    auth: &dyn IbkrAuth,
    symbols: &[String],
) -> anyhow::Result<HashMap<String, i64>> {
    if symbols.is_empty() {
        return Ok(HashMap::new());
    }

    let symbols_param = symbols.join(",");
    let url = format!("{}trsrv/stocks", auth.base_url());
    let full_url = format!("{}?symbols={}", url, symbols_param);

    let auth_header = auth.sign_request(
        "GET",
        &url,
        Some(&[("symbols", &symbols_param)]),
    )?;

    let mut req = http
        .get(&full_url)
        .header("User-Agent", "ibind-rs");

    if let Some(header) = auth_header {
        req = req.header("Authorization", header);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("resolve_conids request failed: {}", e))?;

    let status = resp.status();
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| anyhow::anyhow!("resolve_conids response parse failed: {}", e))?;

    if !status.is_success() {
        return Err(anyhow::anyhow!(
            "resolve_conids returned {}: {}",
            status,
            body
        ));
    }

    // 响应格式: {"AAPL": [{"name": "APPLE INC", "contracts": [{"conid": 265598, "isUS": true, ...}]}]}
    let mut result = HashMap::new();

    let obj = body
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("Expected object in stocks response"))?;

    for (symbol, entries) in obj {
        let entries = match entries.as_array() {
            Some(arr) => arr,
            None => continue,
        };
        if let Some(conid) = select_conid(entries) {
            result.insert(symbol.clone(), conid);
        }
    }

    tracing::info!(
        count = result.len(),
        total = symbols.len(),
        "Resolved IBKR conids"
    );

    for symbol in symbols {
        if !result.contains_key(symbol) {
            tracing::warn!(symbol = %symbol, "Failed to resolve conid for symbol");
        }
    }

    Ok(result)
}

/// 从一个 symbol 的 entries（`{"contracts":[{"conid":..,"isUS":..}]}` 列表）里挑 conid。
///
/// 优先取 `isUS=true` 的合约（美股/ADR，如 SKHY）；若该 symbol 名下没有任何美股合约
/// （如韩股 000660 只在 KRX 上市、`isUS=false`），回退到第一个带 conid 的合约——否则非美
/// 标的永远解析不到 conid，引擎也就订阅不到它的行情。纯函数，便于单测。
fn select_conid(entries: &[serde_json::Value]) -> Option<i64> {
    let mut fallback: Option<i64> = None;
    for entry in entries {
        let contracts = match entry.get("contracts").and_then(|c| c.as_array()) {
            Some(arr) => arr,
            None => continue,
        };
        for contract in contracts {
            let conid = match contract.get("conid").and_then(|v| v.as_i64()) {
                Some(c) => c,
                None => continue,
            };
            let is_us = contract.get("isUS").and_then(|v| v.as_bool()).unwrap_or(false);
            if is_us {
                return Some(conid);
            }
            fallback.get_or_insert(conid);
        }
    }
    fallback
}

#[cfg(test)]
mod tests {
    use super::select_conid;
    use serde_json::json;

    #[test]
    fn prefers_us_contract() {
        // SKHY：只有一个 US ADR 合约
        let entries = json!([{"contracts": [{"conid": 899700992, "isUS": true, "exchange": "NASDAQ"}]}]);
        assert_eq!(select_conid(entries.as_array().unwrap()), Some(899700992));
    }

    #[test]
    fn falls_back_to_non_us_when_no_us() {
        // 000660：只有 KRX 合约，isUS=false —— 旧逻辑会漏掉，新逻辑回退取到
        let entries = json!([{"contracts": [{"conid": 17382246, "isUS": false, "exchange": "KRX"}]}]);
        assert_eq!(select_conid(entries.as_array().unwrap()), Some(17382246));
    }

    #[test]
    fn us_wins_over_non_us_when_both_present() {
        // 同一 symbol 兼有非美与美股合约：仍优先美股，与回退顺序无关
        let entries = json!([{"contracts": [
            {"conid": 111, "isUS": false, "exchange": "LSE"},
            {"conid": 222, "isUS": true, "exchange": "NYSE"}
        ]}]);
        assert_eq!(select_conid(entries.as_array().unwrap()), Some(222));
    }

    #[test]
    fn none_when_no_contract_has_conid() {
        let entries = json!([{"contracts": [{"isUS": true}]}]);
        assert_eq!(select_conid(entries.as_array().unwrap()), None);
        assert_eq!(select_conid(&[]), None);
    }
}
