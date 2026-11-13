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
//! - 入向（行情、成交、订单、持仓、资费流水）只问 [`HlMarkets::symbol_of`]，一步得到内部
//!   symbol；不在目录里的 coin 得到 `None`，一律不进本引擎 —— Fill 流、持仓基线、对账读数、
//!   资费用的是同一条判据，口径不会分叉。剥 dex 前缀是本模块的私事，外面没有「先判断、再剥」
//!   两步可以写漏。
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

use super::codec::{size_step, AssetInfo, MarginMode};
use crate::domain::{Decimal, Exchange, ExchangeError, Price, PriceRule, Symbol, SymbolMeta};
use rust_decimal::RoundingStrategy;
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
    pub sz_decimals: u32,
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
    /// 内部 symbol -> 品种。入向 [`Self::symbol_of`] 也查它（剥前缀后再核对 coin），
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
                if info.is_delisted {
                    continue;
                }
                match info.margin_mode() {
                    Ok(MarginMode::Cross) => {}
                    Ok(MarginMode::IsolatedOnly) => continue,
                    // 不认识的保证金模式不猜成全仓：剔除并留痕（与 BN/OKX 元数据异常的处置一致），
                    // 策略要用它时由投产校验拒绝
                    Err(e) => {
                        tracing::error!(coin = %info.name, error = %e, "HL 品种保证金模式未知，该品种不可交易");
                        continue;
                    }
                }
                // 前缀与所查 dex 不符说明 meta 口径变了，编号和归属都不可信
                if dex_of_coin(&info.name) != u.dex {
                    return Err(format!(
                        "HL dex \"{}\" 的 meta 里出现不属于它的 coin {}",
                        u.dex, info.name
                    ));
                }
                let symbol = strip_dex(&info.name).to_string();
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
            ExchangeError::new(format!(
                "HL 已接入的 dex {:?} 里没有可交易的 {symbol}",
                self.dexes
            ))
        })
    }

    /// 按 HL coin（带 dex 前缀）找品种；不在目录里为 `None`。剥前缀后还要核对完整 coin：
    /// `flx:TSLA` 剥完是 `TSLA`，但目录里的 `TSLA` 是 `xyz:TSLA`，不是同一个品种。
    fn entry_of_coin(&self, coin: &str) -> Option<(&Symbol, &HlAsset)> {
        self.by_symbol
            .get_key_value(strip_dex(coin))
            .filter(|(_, a)| a.coin == coin)
    }

    /// 按 HL coin（带 dex 前缀）找品种；不在目录里报错
    pub(crate) fn asset_of_coin(&self, coin: &str) -> Result<&HlAsset, ExchangeError> {
        self.entry_of_coin(coin).map(|(_, a)| a).ok_or_else(|| {
            ExchangeError::new(format!(
                "HL 已接入的 dex {:?} 里没有可交易的 {coin}",
                self.dexes
            ))
        })
    }

    /// 内部 symbol 对应的 HL coin
    pub(crate) fn coin(&self, symbol: &Symbol) -> Option<&str> {
        self.by_symbol.get(symbol).map(|a| a.coin.as_str())
    }

    /// 入向唯一的换算：HL coin → 内部 symbol；不属于本引擎（不在品种目录里）为 `None`。
    ///
    /// 账户级推送与查询（userFills、orderUpdates、userFunding、clearinghouseState）一次返回该地址
    /// 在**所有** dex 上的记录；单剥前缀会把同名的两个品种认成一个，故「属不属于本引擎」与
    /// 「对应哪个 symbol」在这里一步给出。
    pub(crate) fn symbol_of(&self, coin: &str) -> Option<Symbol> {
        self.entry_of_coin(coin).map(|(s, _)| s.clone())
    }

    /// 全部可交易品种的 SymbolMeta
    pub(crate) fn symbol_metas(&self) -> Vec<SymbolMeta> {
        self.by_symbol
            .iter()
            .map(|(symbol, a)| SymbolMeta {
                exchange: Exchange::Hyperliquid,
                symbol: symbol.clone(),
                price_rule: Arc::new(hl_price_rule(a.sz_decimals)),
                size_step: size_step(a.sz_decimals),
                min_order_size: size_step(a.sz_decimals), // 最小下单量为一个精度单位
                min_notional: MIN_ORDER_NOTIONAL,
                contract_size: Decimal::ONE, // 币本位，合约乘数为 1
            })
            .collect()
    }
}

/// coin 所属的 dex：`xyz:TSLA` → "xyz"，`BTC` → ""（主站）
pub(crate) fn dex_of_coin(coin: &str) -> &str {
    coin.split_once(':').map_or("", |(dex, _)| dex)
}

/// 去掉 dex 前缀：`xyz:TSLA` → `TSLA`，`BTC` → `BTC`。只在本模块内用：入向换算一律经
/// [`HlMarkets::symbol_of`]（连同「在不在目录里」一起判），不单独剥前缀。
fn strip_dex(coin: &str) -> &str {
    coin.split_once(':').map_or(coin, |(_, name)| name)
}

/// `perpDexs` 响应里的全部 perp dex 名（"" = 主站，即列表第 0 项 `null`）。名字解析不了报错：
/// 清场要查遍账户所有 dex 的挂单，少一个就会谎报干净
pub(crate) fn perp_dex_names(
    perp_dexs: &[Option<serde_json::Value>],
) -> Result<Vec<String>, String> {
    perp_dexs
        .iter()
        .map(|e| match e {
            None => Ok(String::new()),
            Some(v) => v
                .get("name")
                .and_then(|n| n.as_str())
                .map(str::to_string)
                .ok_or_else(|| format!("HL perpDexs 条目没有 name：{v}")),
        })
        .collect()
}

/// 由 `perpDexs` 响应求某 dex 的资产编号偏移。列表第 0 项是主站（`null`）。
pub(crate) fn dex_offset(
    perp_dexs: &[Option<serde_json::Value>],
    dex: &str,
) -> Result<u32, String> {
    if dex.is_empty() {
        return Ok(0);
    }
    let idx = perp_dexs
        .iter()
        .position(|e| {
            e.as_ref()
                .and_then(|v| v.get("name"))
                .and_then(|n| n.as_str())
                == Some(dex)
        })
        .ok_or_else(|| format!("HL perpDexs 里没有 dex \"{dex}\""))?;
    Ok(BUILDER_DEX_ASSET_BASE + idx as u32 * BUILDER_DEX_ASSET_STEP)
}

/// HL 价格精度规则（5 位有效数字、小数位 ≤ 6 - szDecimals）。
/// 限价单（经 SymbolMeta）与市价单定价共用这一份规则。
pub(crate) fn hl_price_rule(sz_decimals: u32) -> HlSigFigs {
    HlSigFigs::new(sz_decimals)
}

/// Hyperliquid 永续：至多 5 位有效数字，且小数位不超过 `6 - szDecimals`；整数价格总是合法。
#[derive(Debug, Clone)]
pub(crate) struct HlSigFigs {
    sz_decimals: u32,
}

impl HlSigFigs {
    pub(crate) fn new(sz_decimals: u32) -> Self {
        Self { sz_decimals }
    }
}

impl PriceRule for HlSigFigs {
    fn round(&self, price: Price) -> Price {
        if price <= Decimal::ZERO || price.fract().is_zero() {
            return price;
        }
        // 首位有效数字的十进制指数：12345.6 → 4，0.000251 → -4。
        // normalize 后 price = mantissa × 10^-scale，mantissa 有 n 位，首位指数 = n - 1 - scale
        let p = price.normalize();
        let digits = p.mantissa().unsigned_abs().to_string().len() as i64;
        let lead_exp = digits - 1 - p.scale() as i64;
        let sig_fig_decimals = (4 - lead_exp).max(0) as u32;
        let max_decimals = 6_u32.saturating_sub(self.sz_decimals);
        price.round_dp_with_strategy(
            sig_fig_decimals.min(max_decimals),
            RoundingStrategy::MidpointAwayFromZero,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    /// 清场要查遍的 dex：主站（列表第 0 项 null）+ 各 builder dex，名字解析不了就报错
    #[test]
    fn perp_dex_names_include_main_and_every_builder_dex() {
        let raw: Vec<Option<serde_json::Value>> =
            serde_json::from_str(r#"[null, {"name":"xyz","fullName":"XYZ"}, {"name":"flx"}]"#)
                .unwrap();
        assert_eq!(perp_dex_names(&raw).unwrap(), vec!["", "xyz", "flx"]);
        let broken: Vec<Option<serde_json::Value>> =
            serde_json::from_str(r#"[null, {"fullName":"?"}]"#).unwrap();
        assert!(perp_dex_names(&broken).is_err());
    }

    fn info(name: &str, sz: i32, extra: &str) -> AssetInfo {
        serde_json::from_str(&format!(
            r#"{{"name":"{name}","szDecimals":{sz},"maxLeverage":10{extra}}}"#
        ))
        .unwrap()
    }

    /// 实测 perpDexs 的开头（2026-11）：主站是 null，xyz 下标 1、flx 下标 2
    fn perp_dexs() -> Vec<Option<serde_json::Value>> {
        serde_json::from_str(
            r#"[null,{"name":"xyz","fullName":"XYZ"},{"name":"flx","fullName":"Felix"}]"#,
        )
        .unwrap()
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
                    info(
                        "xyz:STX",
                        2,
                        r#","isDelisted":true,"onlyIsolated":true,"marginMode":"noCross""#,
                    ),
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
        assert_eq!(
            (tsla.asset, tsla.coin.as_str(), tsla.dex.as_str()),
            (110_001, "xyz:TSLA", "xyz")
        );
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

    /// 入向换算：只认目录里的 coin，一步给出剥前缀后的 symbol。同名不同 dex 的（xyz:STX 对
    /// 主站 STX）、未接入 dex 的（flx:TSLA）、已接入 dex 上不可交易的（xyz:AAPL）都不认 ——
    /// 否则剥前缀后会串到同名品种上
    #[test]
    fn symbol_of_only_catalogued_coins() {
        let m = two_dex_markets();
        assert_eq!(m.symbol_of("BTC").as_deref(), Some("BTC"));
        assert_eq!(m.symbol_of("xyz:TSLA").as_deref(), Some("TSLA"));
        assert_eq!(m.symbol_of("STX").as_deref(), Some("STX"));
        for foreign in ["xyz:STX", "flx:TSLA", "TSLA", "xyz:AAPL", "@107"] {
            assert_eq!(m.symbol_of(foreign), None, "{foreign}");
        }
    }

    /// 保证金模式不认识的品种不当全仓收进来（剔除，不影响别的品种）
    #[test]
    fn unknown_margin_mode_is_excluded_not_treated_as_cross() {
        let m = HlMarkets::build(vec![DexUniverse {
            dex: String::new(),
            offset: 0,
            universe: vec![
                info("BTC", 5, ""),
                info("ETH", 4, r#","marginMode":"somethingNew""#),
            ],
        }])
        .unwrap();
        assert!(m.asset(&"BTC".into()).is_ok());
        assert!(m.asset(&"ETH".into()).is_err());
    }

    /// 同名品种在两个已接入 dex 都可交易：启动即报错，不静默挑一个
    #[test]
    fn same_symbol_on_two_connected_dexes_is_rejected() {
        let err = HlMarkets::build(vec![
            DexUniverse {
                dex: String::new(),
                offset: 0,
                universe: vec![info("GOLD", 2, "")],
            },
            DexUniverse {
                dex: "xyz".into(),
                offset: 110_000,
                universe: vec![info("xyz:GOLD", 2, "")],
            },
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

    #[test]
    fn hl_significant_figures() {
        let s = |rule: &dyn PriceRule, p: Decimal| rule.round(p).normalize().to_string();
        let f0 = HlSigFigs::new(0);
        assert_eq!(s(&f0, dec!(0.000251)), "0.000251");
        assert_eq!(s(&f0, dec!(0.00012345)), "0.000123", "受 6 位小数限制");
        let f2 = HlSigFigs::new(2);
        assert_eq!(s(&f2, dec!(0.12345)), "0.1235", "受 max_decimals=4 限制");
        assert_eq!(s(&f2, dec!(1234.5)), "1234.5");
        assert_eq!(s(&f2, dec!(1234.56)), "1234.6");
        assert_eq!(s(&f2, dec!(100000)), "100000");
        assert_eq!(s(&f2, dec!(123456789)), "123456789", "整数价格总是合法");
        let f5 = HlSigFigs::new(5);
        assert_eq!(s(&f5, dec!(106290.4)), "106290");
        assert_eq!(s(&f5, dec!(99999.6)), "100000");
        assert_eq!(s(&f5, dec!(70010.2)), "70010");
        assert_eq!(s(&f5, dec!(12345.6)), "12346");
        let f1 = HlSigFigs::new(1);
        assert_eq!(s(&f1, dec!(0.123456)), "0.12346");
        assert_eq!(s(&f1, dec!(1.000001)), "1");
    }
}
