//! 一个 client 接入的 HL 市场目录：接了哪些 perp dex、每个 dex 上哪些品种可交易、
//! 内部 symbol 与 HL coin 怎么互转、下单用的全局资产编号是多少。
//!
//! # 为什么启动时建一次、之后只读
//!
//! 内部 symbol 不带 dex 前缀（"BTC"、"TSLA"），HL 的 coin 带（"BTC"、"xyz:TSLA"）。
//! 一个 client 接多个 dex 后，「TSLA 在哪个 dex」不能再由配置里的单个 dex 推出来，
//! 只能查各 dex 的 meta。把这件事收在本目录里、启动时建好：
//! - 出向（下单、撤单、订阅、查费率）只问 [`HlMarkets::asset`] / [`HlMarkets::coin`]，
//!   查不到就报错，不会拼出一个不存在的 coin；
//! - 入向（成交、订单、持仓、资费流水）只问 [`HlMarkets::owns`]，不在目录里的 coin 一律
//!   不进本引擎 —— Fill 流、持仓基线、对账读数、资费用的是同一条判据，口径不会分叉。
//!
//! 运行期新上的品种不在目录里，要重启才能交易（与 symbol metas 只在投产期预加载一致）。
//!
//! # 收哪些品种
//!
//! 已接入 dex 上**未下架且支持全仓**的品种（与此前 `fetch_all_symbol_metas` 的口径相同）。
//! 同名冲突只在这个集合里查：实测（2026-11）主站 `STX` 与 xyz 的 `xyz:STX` 同名，但后者
//! 已下架且只许逐仓，不在集合里 —— 若把全部品种都拿来查冲突，进程会因一个永远不会交易的
//! 品种起不来。不在集合里的 coin（包括这个 `xyz:STX`）的成交与持仓不进账本：它们不可能
//! 是本引擎下的单，也不在任何策略的对账范围里。

use super::codec::{size_step, AssetInfo};
use super::symbol::from_hyperliquid;
use crate::domain::{Exchange, ExchangeError, SignificantFiguresFormatter, Symbol, SymbolMeta};
use std::collections::HashMap;
use std::sync::Arc;

/// HIP-3 dex 的资产编号：`100000 + dex 在 perpDexs 列表中的下标 × 10000 + 品种在该 dex
/// meta 里的下标`。主站（下标 0，列表里是 `null`）不加偏移。
/// HL 单笔最小下单金额（USD，所有品种同一条规则：低于它交易所回 "Order must have minimum value of $10"）
const MIN_ORDER_NOTIONAL: f64 = 10.0;
const BUILDER_DEX_ASSET_BASE: u32 = 100_000;
const BUILDER_DEX_ASSET_STEP: u32 = 10_000;

/// 一个可交易品种下单要用的信息
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HlAsset {
    /// HL coin，带 dex 前缀（`BTC`、`xyz:TSLA`）
    pub coin: String,
    /// 所属 dex（"" = 主站）
    pub dex: String,
    /// 全局资产编号（含 dex 偏移）
    pub asset: u32,
    pub sz_decimals: i32,
}

/// 一个 dex 的 meta 与它的资产编号偏移（建目录的原料）
pub(crate) struct DexUniverse {
    pub dex: String,
    pub offset: u32,
    pub universe: Vec<AssetInfo>,
}

/// 见模块文档
#[derive(Debug)]
pub(crate) struct HlMarkets {
    /// 接入的 dex，按配置顺序
    dexes: Vec<String>,
    /// 内部 symbol -> 品种。入向判据 [`Self::owns`] 也查它（按 coin 反查），
    /// 不另存反向表：目录只读、品种数百个，线性查找的代价可忽略，换来只有一份数据。
    by_symbol: HashMap<Symbol, HlAsset>,
}

impl HlMarkets {
    /// 由各 dex 的 meta 建目录。同一 symbol 出现在两个已接入 dex 的可交易集合里即报错，
    /// 不静默挑一个 —— 挑错了下单会下到另一个市场去。
    pub(crate) fn build(universes: Vec<DexUniverse>) -> Result<Self, String> {
        let dexes: Vec<String> = universes.iter().map(|u| u.dex.clone()).collect();
        let mut by_symbol: HashMap<Symbol, HlAsset> = HashMap::new();
        for u in universes {
            for (idx, info) in u.universe.iter().enumerate() {
                if info.is_delisted || !info.supports_cross_margin() {
                    continue;
                }
                // 前缀与所查 dex 不符说明 meta 口径变了，编号和归属都不可信
                if dex_of_coin(&info.name) != u.dex {
                    return Err(format!(
                        "HL dex \"{}\" 的 meta 里出现不属于它的 coin {}",
                        u.dex, info.name
                    ));
                }
                let symbol = from_hyperliquid(&info.name);
                let asset = HlAsset {
                    coin: info.name.clone(),
                    dex: u.dex.clone(),
                    asset: u.offset + idx as u32,
                    sz_decimals: info.sz_decimals,
                };
                if let Some(prev) = by_symbol.insert(symbol.clone(), asset) {
                    return Err(format!(
                        "HL 已接入的 dex 里 {symbol} 同名：{} 与 {}；一个进程不能同时接这两个市场的同名品种",
                        prev.coin, info.name
                    ));
                }
            }
        }
        Ok(Self { dexes, by_symbol })
    }

    /// 接入的 dex（"" = 主站），按配置顺序
    pub(crate) fn dexes(&self) -> &[String] {
        &self.dexes
    }

    /// 下单 / 撤单用的品种信息；不在目录里报错
    pub(crate) fn asset(&self, symbol: &Symbol) -> Result<&HlAsset, ExchangeError> {
        self.by_symbol.get(symbol).ok_or_else(|| {
            ExchangeError::Other(format!(
                "HL 已接入的 dex {:?} 里没有可交易的 {symbol}",
                self.dexes
            ))
        })
    }

    /// 内部 symbol 对应的 HL coin
    pub(crate) fn coin(&self, symbol: &Symbol) -> Option<&str> {
        self.by_symbol.get(symbol).map(|a| a.coin.as_str())
    }

    /// 入向判据：这个 coin 的成交 / 订单 / 持仓 / 资费是否属于本引擎。
    ///
    /// 账户级推送与查询（userFills、orderUpdates、userFunding）一次返回该地址在**所有**
    /// dex 上的记录；剥掉前缀后同名的两个品种会被认成一个，故入口一律先过这道判据。
    pub(crate) fn owns(&self, coin: &str) -> bool {
        self.by_symbol
            .get(&from_hyperliquid(coin))
            .is_some_and(|a| a.coin == coin)
    }

    /// 全部可交易品种的 SymbolMeta
    pub(crate) fn symbol_metas(&self) -> Vec<SymbolMeta> {
        self.by_symbol
            .iter()
            .map(|(symbol, a)| SymbolMeta {
                exchange: Exchange::Hyperliquid,
                symbol: symbol.clone(),
                price_formatter: Arc::new(hl_price_formatter(a.sz_decimals)),
                size_step: size_step(a.sz_decimals),
                min_order_size: size_step(a.sz_decimals), // 最小下单量为一个精度单位
                min_notional: MIN_ORDER_NOTIONAL,
                contract_size: 1.0,                       // 币本位，合约乘数为 1
            })
            .collect()
    }
}

/// coin 所属的 dex：`xyz:TSLA` → "xyz"，`BTC` → ""（主站）
pub(crate) fn dex_of_coin(coin: &str) -> &str {
    coin.split_once(':').map_or("", |(dex, _)| dex)
}

/// 由 `perpDexs` 响应求某 dex 的资产编号偏移。列表第 0 项是主站（`null`）。
pub(crate) fn dex_offset(perp_dexs: &[Option<serde_json::Value>], dex: &str) -> Result<u32, String> {
    if dex.is_empty() {
        return Ok(0);
    }
    let idx = perp_dexs
        .iter()
        .position(|e| e.as_ref().and_then(|v| v.get("name")).and_then(|n| n.as_str()) == Some(dex))
        .ok_or_else(|| format!("HL perpDexs 里没有 dex \"{dex}\""))?;
    Ok(BUILDER_DEX_ASSET_BASE + idx as u32 * BUILDER_DEX_ASSET_STEP)
}

/// 按 HL 规则（5 位有效数字、小数位 ≤ 6 - szDecimals）格式化价格的格式化器。
/// 限价单（经 SymbolMeta）与市价单定价共用这一份规则。
pub(crate) fn hl_price_formatter(sz_decimals: i32) -> SignificantFiguresFormatter {
    SignificantFiguresFormatter::new(sz_decimals.max(0) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(name: &str, sz: i32, extra: &str) -> AssetInfo {
        serde_json::from_str(&format!(
            r#"{{"name":"{name}","szDecimals":{sz},"maxLeverage":10{extra}}}"#
        ))
        .unwrap()
    }

    /// 实测 perpDexs 的开头（2026-11）：主站是 null，xyz 下标 1、flx 下标 2
    fn perp_dexs() -> Vec<Option<serde_json::Value>> {
        serde_json::from_str(r#"[null,{"name":"xyz","fullName":"XYZ"},{"name":"flx","fullName":"Felix"}]"#).unwrap()
    }

    fn two_dex_markets() -> HlMarkets {
        let pd = perp_dexs();
        HlMarkets::build(vec![
            DexUniverse {
                dex: String::new(),
                offset: dex_offset(&pd, "").unwrap(),
                universe: vec![info("BTC", 5, ""), info("ETH", 4, ""), info("STX", 1, "")],
            },
            DexUniverse {
                dex: "xyz".into(),
                offset: dex_offset(&pd, "xyz").unwrap(),
                universe: vec![
                    info("xyz:XYZ100", 4, ""),
                    info("xyz:TSLA", 3, ""),
                    // 实测：已下架且只许逐仓，与主站 STX 同名
                    info("xyz:STX", 2, r#","isDelisted":true,"onlyIsolated":true,"marginMode":"noCross""#),
                    info("xyz:AAPL", 3, r#","marginMode":"noCross""#),
                ],
            },
        ])
        .unwrap()
    }

    #[test]
    fn asset_index_carries_dex_offset() {
        let pd = perp_dexs();
        assert_eq!(dex_offset(&pd, "").unwrap(), 0);
        assert_eq!(dex_offset(&pd, "xyz").unwrap(), 110_000);
        assert_eq!(dex_offset(&pd, "flx").unwrap(), 120_000);
        assert!(dex_offset(&pd, "nope").is_err());

        let m = two_dex_markets();
        assert_eq!(m.asset(&"ETH".into()).unwrap().asset, 1);
        let tsla = m.asset(&"TSLA".into()).unwrap();
        assert_eq!((tsla.asset, tsla.coin.as_str(), tsla.dex.as_str()), (110_001, "xyz:TSLA", "xyz"));
    }

    #[test]
    fn symbols_map_to_prefixed_coins_across_dexes() {
        let m = two_dex_markets();
        assert_eq!(m.coin(&"BTC".into()), Some("BTC"));
        assert_eq!(m.coin(&"TSLA".into()), Some("xyz:TSLA"));
        // 只许逐仓的不收
        assert_eq!(m.coin(&"AAPL".into()), None);
        assert!(m.asset(&"AAPL".into()).is_err());
        let mut names: Vec<_> = m.symbol_metas().into_iter().map(|s| s.symbol).collect();
        names.sort();
        assert_eq!(names, ["BTC", "ETH", "STX", "TSLA", "XYZ100"]);
    }

    /// 入向判据：只认目录里的 coin。同名不同 dex 的（xyz:STX 对主站 STX）、未接入 dex 的
    /// （flx:TSLA）、已接入 dex 上不可交易的（xyz:AAPL）都不认 —— 否则剥前缀后会串到同名品种上
    #[test]
    fn owns_only_catalogued_coins() {
        let m = two_dex_markets();
        assert!(m.owns("BTC"));
        assert!(m.owns("xyz:TSLA"));
        assert!(m.owns("STX"));
        assert!(!m.owns("xyz:STX"));
        assert!(!m.owns("flx:TSLA"));
        assert!(!m.owns("TSLA"));
        assert!(!m.owns("xyz:AAPL"));
    }

    /// 同名品种在两个已接入 dex 都可交易：启动即报错，不静默挑一个
    #[test]
    fn same_symbol_on_two_connected_dexes_is_rejected() {
        let err = HlMarkets::build(vec![
            DexUniverse { dex: String::new(), offset: 0, universe: vec![info("GOLD", 2, "")] },
            DexUniverse { dex: "xyz".into(), offset: 110_000, universe: vec![info("xyz:GOLD", 2, "")] },
        ])
        .unwrap_err();
        assert!(err.contains("GOLD") && err.contains("xyz:GOLD"), "{err}");
    }

    #[test]
    fn coin_prefix_must_match_its_dex() {
        let err = HlMarkets::build(vec![DexUniverse {
            dex: "xyz".into(),
            offset: 110_000,
            universe: vec![info("TSLA", 2, "")],
        }])
        .unwrap_err();
        assert!(err.contains("不属于它"), "{err}");
    }
}
