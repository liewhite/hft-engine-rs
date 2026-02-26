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

        for entry in entries {
            let contracts = match entry.get("contracts").and_then(|c| c.as_array()) {
                Some(arr) => arr,
                None => continue,
            };

            // 过滤 isUS=true 的合约，取第一个
            for contract in contracts {
                let is_us = contract.get("isUS").and_then(|v| v.as_bool()).unwrap_or(false);
                if !is_us {
                    continue;
                }
                if let Some(conid) = contract.get("conid").and_then(|v| v.as_i64()) {
                    result.insert(symbol.clone(), conid);
                    break;
                }
            }

            // 找到就停止遍历 entries
            if result.contains_key(symbol) {
                break;
            }
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
