//! IBKR symbol ↔ conid 解析
//!
//! 通过 REST API 将 symbol 列表解析为 conid 映射

use super::auth::IbkrAuth;
use reqwest::Client;
use std::collections::HashMap;

/// 通过 REST API 批量解析 symbols 到 conid 映射
///
/// GET /trsrv/stocks?symbols=AAPL,NVDA,...
/// 每个 symbol 的合约选择见 [`select_conid`]：优先 isUS 美股/ADR；无美股合约时回退唯一的
/// 非美合约（如韩股 000660→KRX）；跨多个非美市场无法判别时拒绝解析（不猜、不订错市场）。
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
        match select_conid(entries) {
            ConidChoice::Us { conid } => {
                result.insert(symbol.clone(), conid);
            }
            ConidChoice::NonUsFallback { conid, exchange } => {
                // 回退命中要可诊断：日志带 symbol/conid/交易所，避免"订到哪个市场"无迹可查
                tracing::info!(
                    symbol = %symbol,
                    conid,
                    exchange = %exchange.as_deref().unwrap_or("?"),
                    "Resolved IBKR conid via non-US fallback"
                );
                result.insert(symbol.clone(), conid);
            }
            ConidChoice::Ambiguous { exchanges } => {
                // 跨多个非美市场，无法判别该订哪个——宁可不解析也不猜（猜错=订错市场行情）
                tracing::warn!(
                    symbol = %symbol,
                    exchanges = ?exchanges,
                    "Multiple non-US listings for symbol, refusing to guess conid"
                );
            }
            ConidChoice::None => {}
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

/// 一个 symbol 的合约选择结果。
#[derive(Debug, PartialEq)]
enum ConidChoice {
    /// 命中 isUS 美股/ADR 合约
    Us { conid: i64 },
    /// 无美股合约、回退到唯一的非美合约（如韩股 000660→KRX）
    NonUsFallback { conid: i64, exchange: Option<String> },
    /// 跨多个非美市场、无法判别——拒绝解析（不猜、不订错市场）
    Ambiguous { exchanges: Vec<String> },
    /// 无任何带 conid 的合约
    None,
}

/// 从一个 symbol 的 entries（`{"contracts":[{"conid":..,"isUS":..,"exchange":..}]}` 列表）里挑 conid。
///
/// 规则：优先取 `isUS=true` 的合约（美股/ADR，如 SKHY，遍历顺序无关，一遇即返回）；无美股合约时，
/// 若非美合约**唯一**（按 conid 去重后只剩一个，如韩股 000660 只在 KRX）则回退取它；若跨多个非美
/// 市场则判为 `Ambiguous` 拒绝解析——绝不"取第一个"，否则会静默订到错误市场的行情。纯函数，便于单测。
fn select_conid(entries: &[serde_json::Value]) -> ConidChoice {
    // 非美候选（保序），isUS 命中直接返回
    let mut non_us: Vec<(i64, Option<String>)> = Vec::new();
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
                return ConidChoice::Us { conid };
            }
            let exchange = contract
                .get("exchange")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            non_us.push((conid, exchange));
        }
    }

    // 无美股合约：按 conid 去重决定唯一 / 歧义
    let distinct: std::collections::BTreeSet<i64> = non_us.iter().map(|(c, _)| *c).collect();
    match distinct.len() {
        0 => ConidChoice::None,
        1 => {
            let (conid, exchange) = non_us.into_iter().next().unwrap();
            ConidChoice::NonUsFallback { conid, exchange }
        }
        _ => ConidChoice::Ambiguous {
            exchanges: non_us.into_iter().filter_map(|(_, e)| e).collect(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{select_conid, ConidChoice};
    use serde_json::json;

    fn pick(v: serde_json::Value) -> ConidChoice {
        select_conid(v.as_array().unwrap())
    }

    #[test]
    fn prefers_us_contract() {
        // SKHY：只有一个 US ADR 合约
        let c = pick(json!([{"contracts": [{"conid": 899700992, "isUS": true, "exchange": "NASDAQ"}]}]));
        assert_eq!(c, ConidChoice::Us { conid: 899700992 });
    }

    #[test]
    fn falls_back_to_unique_non_us() {
        // 000660：只有 KRX 合约，isUS=false —— 旧逻辑会漏掉，新逻辑回退取到，且带交易所
        let c = pick(json!([{"contracts": [{"conid": 17382246, "isUS": false, "exchange": "KRX"}]}]));
        assert_eq!(
            c,
            ConidChoice::NonUsFallback { conid: 17382246, exchange: Some("KRX".into()) }
        );
    }

    #[test]
    fn us_wins_over_non_us_regardless_of_order() {
        // 非美合约排在美股前面，仍优先美股（一遇 isUS 即返回，与顺序无关）
        let c = pick(json!([{"contracts": [
            {"conid": 111, "isUS": false, "exchange": "LSE"},
            {"conid": 222, "isUS": true, "exchange": "NYSE"}
        ]}]));
        assert_eq!(c, ConidChoice::Us { conid: 222 });
    }

    #[test]
    fn multiple_non_us_markets_are_ambiguous() {
        // 跨 KRX / HKEX 两个非美市场、conid 不同 → 拒绝猜，判 Ambiguous
        let c = pick(json!([{"contracts": [
            {"conid": 17382246, "isUS": false, "exchange": "KRX"},
            {"conid": 99999999, "isUS": false, "exchange": "SEHK"}
        ]}]));
        match c {
            ConidChoice::Ambiguous { exchanges } => {
                assert!(exchanges.contains(&"KRX".to_string()));
                assert!(exchanges.contains(&"SEHK".to_string()));
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn same_non_us_conid_across_entries_not_ambiguous() {
        // 多个 entry 但同一 conid（同合约不同行）→ 去重后唯一，仍回退
        let c = pick(json!([
            {"contracts": [{"conid": 17382246, "isUS": false, "exchange": "KRX"}]},
            {"contracts": [{"conid": 17382246, "isUS": false, "exchange": "KRX"}]}
        ]));
        assert_eq!(
            c,
            ConidChoice::NonUsFallback { conid: 17382246, exchange: Some("KRX".into()) }
        );
    }

    #[test]
    fn entry_without_contracts_is_skipped() {
        // 某 entry 缺 contracts 字段应跳过，不影响另一 entry 的解析
        let c = pick(json!([
            {"name": "no contracts here"},
            {"contracts": [{"conid": 17382246, "isUS": false, "exchange": "KRX"}]}
        ]));
        assert_eq!(
            c,
            ConidChoice::NonUsFallback { conid: 17382246, exchange: Some("KRX".into()) }
        );
    }

    #[test]
    fn none_when_no_contract_has_conid() {
        assert_eq!(pick(json!([{"contracts": [{"isUS": true}]}])), ConidChoice::None);
        assert_eq!(select_conid(&[]), ConidChoice::None);
    }
}
