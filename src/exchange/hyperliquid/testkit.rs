//! 测试用：HL 真实响应样例 + 适配层内部纯函数的薄入口（供引擎层的跨层测试使用，
//! 如持仓对账在多 dex 下的端到端用例），只在 `cfg(test)` 下编译。

use super::client::positions_from_states;
use super::codec::{AssetInfo, ClearinghouseState};
use super::markets::{DexUniverse, HlMarkets};
use crate::domain::Position;
use crate::messaging::AccountEvent;

/// 2026-11-12 实测，某统一账户（地址已抹去）`spotClearinghouseState`
pub(crate) const UNIFIED_SPOT: &str = r#"{"balances":[{"coin":"USDC","token":0,"total":"73788.12197337","hold":"58536.864285","entryNtl":"0.0"}],"tokenToAvailableAfterMaintenance":[[0,"53721.87197337"]]}"#;
/// 同一账户同一时刻 `clearinghouseState(dex="")`：两个全仓仓位
pub(crate) const UNIFIED_MAIN: &str = r#"{"marginSummary":{"accountValue":"33821.87","totalNtlPos":"247905.0","totalRawUsd":"53036.87","totalMarginUsed":"24790.5"},"crossMarginSummary":{"accountValue":"33821.87","totalNtlPos":"247905.0","totalRawUsd":"53036.87","totalMarginUsed":"24790.5"},"crossMaintenanceMarginUsed":"12395.25","withdrawable":"69.37","assetPositions":[{"type":"oneWay","position":{"coin":"kPEPE","szi":"-30000000.0","leverage":{"type":"cross","value":10},"entryPx":"0.004974","positionValue":"133560.0","unrealizedPnl":"15662.786056","returnOnEquity":"1.049624288","liquidationPx":"0.006157607","marginUsed":"13356.0","maxLeverage":10,"cumFunding":{"allTime":"-89.879237","sinceOpen":"-89.879237","sinceChange":"-89.879237"}}},{"type":"oneWay","position":{"coin":"BCH","szi":"500.0","leverage":{"type":"cross","value":10},"entryPx":"228.789","positionValue":"114345.0","unrealizedPnl":"-49.94157","returnOnEquity":"-0.0043657149","liquidationPx":"115.5813221621","marginUsed":"11434.5","maxLeverage":10,"cumFunding":{"allTime":"-11.477352","sinceOpen":"-11.477352","sinceChange":"-11.449094"}}}],"time":1794470673248}"#;
/// 同一账户同一时刻 `clearinghouseState(dex="xyz")`：一个全仓股票仓位
pub(crate) const UNIFIED_XYZ: &str = r#"{"marginSummary":{"accountValue":"24767.444285","totalNtlPos":"306850.0","totalRawUsd":"-282082.555715","totalMarginUsed":"15342.5"},"crossMarginSummary":{"accountValue":"24767.444285","totalNtlPos":"306850.0","totalRawUsd":"-282082.555715","totalMarginUsed":"15342.5"},"crossMaintenanceMarginUsed":"7671.25","withdrawable":"164.801428","assetPositions":[{"type":"oneWay","position":{"coin":"xyz:NVDA","szi":"1900.0","leverage":{"type":"cross","value":20},"entryPx":"164.795","positionValue":"306850.0","unrealizedPnl":"-6260.5","returnOnEquity":"-0.3998907734","liquidationPx":"132.4977749134","marginUsed":"15342.5","maxLeverage":20,"cumFunding":{"allTime":"342.273414","sinceOpen":"342.273414","sinceChange":"0.0"}}}],"time":1794470673742}"#;
/// 2026-11-12 实测，本策略用的统一账户（无仓位）`spotClearinghouseState`
pub(crate) const OWN_SPOT: &str = r#"{"balances":[{"coin":"USDC","token":0,"total":"996.960256","hold":"0.0","entryNtl":"0.0"}],"tokenToAvailableAfterMaintenance":[[0,"996.960256"]]}"#;
/// 同一账户 `clearinghouseState`（主站与 xyz 返回相同）：统一账户下净值恒为 0
pub(crate) const OWN_EMPTY_STATE: &str = r#"{"marginSummary":{"accountValue":"0.0","totalNtlPos":"0.0","totalRawUsd":"0.0","totalMarginUsed":"0.0"},"crossMarginSummary":{"accountValue":"0.0","totalNtlPos":"0.0","totalRawUsd":"0.0","totalMarginUsed":"0.0"},"crossMaintenanceMarginUsed":"0.0","withdrawable":"0.0","assetPositions":[],"time":1794470179552}"#;

fn info(name: &str) -> AssetInfo {
    serde_json::from_str(&format!(
        r#"{{"name":"{name}","szDecimals":2,"maxLeverage":10}}"#
    ))
    .unwrap()
}

/// 品种目录：主站 BTC / kPEPE / BCH，xyz 有 NVDA / TSLA；只建 `dexes` 里列出的
pub(crate) fn markets(dexes: &[&str]) -> HlMarkets {
    HlMarkets::build(
        dexes
            .iter()
            .map(|d| match *d {
                "" => DexUniverse {
                    dex: String::new(),
                    offset: 0,
                    universe: vec![info("BTC"), info("kPEPE"), info("BCH")],
                },
                "xyz" => DexUniverse {
                    dex: "xyz".into(),
                    offset: 110_000,
                    universe: vec![info("xyz:NVDA"), info("xyz:TSLA")],
                },
                other => panic!("testkit 没有 dex {other} 的样例"),
            })
            .collect(),
    )
    .unwrap()
}

pub(crate) fn state(raw: &str) -> ClearinghouseState {
    serde_json::from_str(raw).unwrap()
}

/// 与 `HyperliquidAccount::fetch_positions` 同一份逻辑
pub(crate) fn positions(markets: &HlMarkets, states: &[(&str, &str)]) -> Vec<Position> {
    let states: Vec<_> = states
        .iter()
        .map(|(d, raw)| (d.to_string(), state(raw)))
        .collect();
    positions_from_states(markets, &states).unwrap()
}

/// 一份 clearinghouseState 响应（格式同 [`UNIFIED_MAIN`]），仓位为 `(coin, szi)`
pub(crate) fn state_json(positions: &[(&str, f64)]) -> String {
    let ps: Vec<String> = positions
        .iter()
        .map(|(coin, szi)| {
            format!(
                r#"{{"type":"oneWay","position":{{"coin":"{coin}","szi":"{szi}","leverage":{{"type":"cross","value":10}},"entryPx":"100.0","positionValue":"{pv}","unrealizedPnl":"0.0","returnOnEquity":"0.0","liquidationPx":null,"marginUsed":"1.0","maxLeverage":10,"cumFunding":{{"allTime":"0.0","sinceOpen":"0.0","sinceChange":"0.0"}}}}}}"#,
                pv = szi.abs() * 100.0
            )
        })
        .collect();
    OWN_EMPTY_STATE.replace(
        r#""assetPositions":[]"#,
        &format!(r#""assetPositions":[{}]"#, ps.join(",")),
    )
}

/// 一条 userFills 推送（格式同真实推送），成交为 `(coin, side "B"/"A", sz)`
pub(crate) fn user_fills_json(fills: &[(&str, &str, f64)]) -> String {
    let fs: Vec<String> = fills
        .iter()
        .enumerate()
        .map(|(i, (coin, side, sz))| {
            format!(
                r#"{{"coin":"{coin}","px":"100.0","sz":"{sz}","side":"{side}","time":{t},"startPosition":"0.0","dir":"Open Long","closedPnl":"0.0","hash":"0x0","oid":{i},"crossed":true,"fee":"0.01","tid":{i},"feeToken":"USDC"}}"#,
                t = 1_794_470_000_000u64 + i as u64
            )
        })
        .collect();
    format!(
        r#"{{"channel":"userFills","data":{{"user":"0xabc","fills":[{}]}}}}"#,
        fs.join(",")
    )
}

/// 与私有 WS 同一份解析：一条推送 → 进账本的事件（本地接收时刻记为 `local_ts`）
pub(crate) fn fills_from_ws(
    markets: &HlMarkets,
    raw: &str,
    local_ts: u64,
) -> Result<Vec<AccountEvent>, crate::domain::ExchangeError> {
    super::actor::private_ws::parse_user_fills_for_test(markets, raw, local_ts)
}
