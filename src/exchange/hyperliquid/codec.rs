//! Hyperliquid 数据编解码
//!
//! 解析 Hyperliquid REST API 和 WebSocket 消息。只声明用得上的字段：声明了却不用的必填字段，
//! 交易所哪天改名或去掉它，整条报文就解析不了 —— 持仓基线、对账、私有流会因此停机。
//!
//! coin → 内部 symbol 不在这里换：由调用方经 [`super::markets::HlMarkets::symbol_of`] 一步得到（不在
//! 品种目录里的不进本引擎），再传给这里的转换函数。

use super::markets::HlMarkets;
use crate::domain::{
    now_ms, Decimal, Exchange, Fill, FundingRate, IndexPrice, MarkPrice, MarketTrade, Quantity,
    Side, Symbol, BBO,
};
use indexmap::IndexMap;
use serde::Deserialize;
use std::str::FromStr;

/// Hyperliquid 全站方向编码：`"B"` = bid/买、`"A"` = ask/卖。
///
/// 三处推送共用这套编码，但**语义各不相同**：订单/成交推送里是"本账户这一笔的方向"，
/// trades 里是"主动方 (taker) 的方向"。故此处只统一**编码**解析，语义由各调用方自行表达
/// —— 合并成一个 "parse_taker_side" 会把两种含义混为一谈。
pub(crate) fn parse_side(raw: &str) -> Result<Side, String> {
    match raw {
        "B" => Ok(Side::Long),
        "A" => Ok(Side::Short),
        other => Err(format!("Unknown Hyperliquid side: {}", other)),
    }
}

// ============================================================================
// REST API 响应结构
// ============================================================================

/// Meta 响应 (交易对元数据)
#[derive(Debug, Deserialize)]
pub struct MetaResponse {
    pub universe: Vec<AssetInfo>,
    /// 该 dex 的保证金币种（spot token 编号，USDC = 0）
    #[serde(rename = "collateralToken")]
    pub collateral_token: u32,
}

/// 单个资产信息
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetInfo {
    /// 币种名 (e.g., "BTC", "xyz:TSLA")
    pub name: String,
    /// 数量小数位数
    pub sz_decimals: u32,
    /// 是否已下架
    #[serde(default)]
    pub is_delisted: bool,
    /// 保证金模式，见 [`Self::margin_mode`]
    margin_mode: Option<String>,
}

/// 品种允许的保证金模式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MarginMode {
    /// 可以全仓（字段缺省）
    Cross,
    /// 只许逐仓（`strictIsolated` / `noCross`）
    IsolatedOnly,
}

impl AssetInfo {
    /// 2026-11 实查主站与 xyz 的 meta，只出现过缺省、`strictIsolated`、`noCross` 三种。
    /// 别的值不猜（猜成全仓会在不许全仓的品种上下单），报错由调用方剔除该品种。
    pub(crate) fn margin_mode(&self) -> Result<MarginMode, String> {
        match self.margin_mode.as_deref() {
            None => Ok(MarginMode::Cross),
            Some("strictIsolated") | Some("noCross") => Ok(MarginMode::IsolatedOnly),
            Some(other) => Err(format!("未知的 marginMode: {other}")),
        }
    }
}

/// 资产上下文（资金费率、标记价、oracle 价）
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetCtx {
    /// 当前资金费率
    pub funding: String,
    /// 标记价格
    pub mark_px: String,
    /// oracle 价格
    pub oracle_px: String,
}

// ============================================================================
// WebSocket 消息结构
// ============================================================================

/// BBO 数据
/// API 格式: { coin, time, bbo: [bid_level | null, ask_level | null] }
#[derive(Debug, Deserialize)]
pub struct WsBbo {
    pub coin: String,
    pub time: u64,
    /// [bid, ask] - 每个可能为 null
    pub bbo: [Option<WsLevel>; 2],
}

/// BBO Level (价格层)
#[derive(Debug, Deserialize)]
pub struct WsLevel {
    /// 价格
    pub px: String,
    /// 数量
    pub sz: String,
}

impl WsBbo {
    /// 返回 `Ok(None)` = **单边盘口**（bid 或 ask 为 null）：稀薄品种的合法市场状态，
    /// 不是坏报文 —— 调用方跳过即可。此前按解析错误返回 Err，会经 fail-fast 链路
    /// kill actor、级联整机停机。`Err` 只留给真正的报文损坏。
    pub fn to_bbo(&self, symbol: Symbol) -> Result<Option<BBO>, String> {
        let (Some(bid), Some(ask)) = (self.bbo[0].as_ref(), self.bbo[1].as_ref()) else {
            return Ok(None); // 单边盘口
        };

        let bid_price = crate::domain::parse_decimal(&bid.px)
            .map_err(|_| format!("Failed to parse bid price: {}", bid.px))?;
        let bid_qty = crate::domain::parse_decimal(&bid.sz)
            .map_err(|_| format!("Failed to parse bid size: {}", bid.sz))?;
        let ask_price = crate::domain::parse_decimal(&ask.px)
            .map_err(|_| format!("Failed to parse ask price: {}", ask.px))?;
        let ask_qty = crate::domain::parse_decimal(&ask.sz)
            .map_err(|_| format!("Failed to parse ask size: {}", ask.sz))?;

        Ok(Some(BBO {
            exchange: Exchange::Hyperliquid,
            symbol,
            bid_price,
            bid_qty,
            ask_price,
            ask_qty,
            timestamp: self.time,
        }))
    }
}

/// 成交数据 (trades 订阅)
///
/// API 格式: `{ coin, side, px, sz, time, hash, tid, users }`，本结构只声明所需字段。
///
/// `side` 是**主动方 (taker)** 方向，编码沿用 Hyperliquid 全站约定 `"B"` = 买、`"A"` = 卖
/// (与 [`WsFill`] / 订单推送一致，见 [`parse_side`])。官方文档此处写作 "Buy"/"Sell"，但实测
/// 线路上是 `"B"`/`"A"`；未观测到的形式不做预留容错，由线路一致性测试守住。
///
/// `users` 为 `[buyer, seller]`，据此可定位**主动方地址**：主动买时是 `users[0]`，
/// 主动卖时是 `users[1]`。用于 [`aggregate_trades`] 的归集。
///
/// **本所线路上不归集**：同一主动单吃穿多档会拆成多条。为让上层看到与 Binance aggTrade /
/// OKX trades 一致的口径，归集在本适配层完成 —— 见 [`aggregate_trades`]。
#[derive(Debug, Deserialize)]
pub struct WsTrade {
    pub coin: String,
    pub side: String,
    pub px: String,
    pub sz: String,
    pub time: u64,
    /// L1 交易哈希；尚未上链的撮合为全 0，不能用于识别主动单
    #[serde(default)]
    pub hash: Option<String>,
    /// `[buyer, seller]`
    #[serde(default)]
    pub users: Option<Vec<String>>,
}

impl WsTrade {
    /// 主动方 (taker) 地址：主动买 -> buyer，主动卖 -> seller。
    fn taker(&self, side: Side) -> Option<&str> {
        let users = self.users.as_ref()?;
        let idx = match side {
            Side::Long => 0,  // 主动买，taker 是 buyer
            Side::Short => 1, // 主动卖，taker 是 seller
        };
        users.get(idx).map(|s| s.as_str())
    }

    /// 非全 0 的 hash 才能标识一笔主动单
    fn taker_order_hash(&self) -> Option<&str> {
        let h = self.hash.as_deref()?;
        h.trim_start_matches("0x")
            .bytes()
            .any(|b| b != b'0')
            .then_some(h)
    }
}

/// 把一批线路成交归集为 **aggTrade 口径**：同一主动单在同一价位的多笔撮合合并为一条。
/// coin 不在品种目录里即报错（公共流只订阅目录内的品种，推来别的说明订阅记账出了错）。
///
/// Hyperliquid 逐笔下发，而 Binance/OKX 下发的已是归集结果。若不在此归一，"成交笔数 /
/// 单笔均量 / 到达强度"这类因子跨所不可比 —— 故归集放在适配层，让 domain 口径统一
/// (与数量一律币本位同理，差异由适配层吸收，不外泄给策略)。
///
/// 归集键为 (主动单 hash, 主动方地址, 价格, 主动方向)：hash 可用时最精确；hash 为全 0
/// (尚未上链) 时退化为按主动方地址归集。数量求和，时间取该组最后一笔。
/// 只在**单条 WS 消息内**归集：不缓冲、不等待后续消息，故**零额外延迟**。其完整性依赖
/// "一笔主动单的成交同批下发"，这与 L1 一次撮合原子产出全部 fill 的语义一致，并已实测验证
/// (90s / 109 条消息 / 115 个非零 hash，无一跨消息)。该前提由
/// `crate::exchange::trades_conformance` 中的断言守住。
///
/// 若该前提将来被破坏，后果是**笔数偏高**（一单被计为多条），价格与成交量仍然精确 ——
/// 宁可接受这个有界误差，也不为归集去等时间窗口（那才是延迟敏感策略无法接受的）。
///
/// 用 [`IndexMap`] 保持首次出现顺序，保证同一输入必得同一输出顺序 (回测确定性)。
pub fn aggregate_trades(
    trades: &[WsTrade],
    markets: &HlMarkets,
) -> Result<Vec<MarketTrade>, String> {
    let mut out: IndexMap<(Option<String>, Option<String>, Decimal, bool), MarketTrade> =
        IndexMap::new();

    for t in trades {
        let symbol = markets
            .symbol_of(&t.coin)
            .ok_or_else(|| format!("HL trades 推来品种目录外的 coin: {}", t.coin))?;
        let taker_side = parse_side(&t.side)?;
        // 主动卖 (A) 时买方为挂单方
        let is_buyer_maker = taker_side == Side::Short;
        let price = crate::domain::parse_decimal(&t.px)
            .map_err(|_| format!("Failed to parse trade price: {}", t.px))?;
        let qty = crate::domain::parse_decimal(&t.sz)
            .map_err(|_| format!("Failed to parse trade size: {}", t.sz))?;

        let key = (
            t.taker_order_hash().map(str::to_string),
            t.taker(taker_side).map(str::to_string),
            price,
            is_buyer_maker,
        );
        out.entry(key)
            .and_modify(|agg| {
                agg.qty += qty;
                agg.timestamp = agg.timestamp.max(t.time);
            })
            .or_insert_with(|| MarketTrade {
                exchange: Exchange::Hyperliquid,
                symbol,
                price,
                qty,
                is_buyer_maker,
                timestamp: t.time,
            });
    }

    Ok(out.into_values().collect())
}

/// ActiveAssetCtx 数据 (实时资产上下文)
#[derive(Debug, Deserialize)]
pub struct WsActiveAssetCtx {
    pub coin: String,
    pub ctx: AssetCtx,
}

impl WsActiveAssetCtx {
    /// 转换为 FundingRate
    /// Hyperliquid 每小时整点结算
    /// timestamp: 数据时间戳（毫秒）
    pub fn to_funding_rate(&self, symbol: Symbol, timestamp: u64) -> Result<FundingRate, String> {
        let rate = f64::from_str(&self.ctx.funding)
            .map_err(|_| format!("Failed to parse funding rate: {}", self.ctx.funding))?;

        Ok(FundingRate {
            exchange: Exchange::Hyperliquid,
            symbol,
            rate,
            // Hyperliquid 每小时整点结算，计算下一个整点时间
            next_settle_time: next_hourly_settle_time(),
            timestamp,
        })
    }

    /// 转换为 MarkPrice
    pub fn to_mark_price(&self, symbol: Symbol, timestamp: u64) -> Result<MarkPrice, String> {
        let price = crate::domain::parse_decimal(&self.ctx.mark_px)
            .map_err(|_| format!("Failed to parse mark_px: {}", self.ctx.mark_px))?;

        Ok(MarkPrice {
            exchange: Exchange::Hyperliquid,
            symbol,
            price,
            timestamp,
        })
    }

    /// 转换为 IndexPrice (使用 oracle_px 作为指数价格)
    pub fn to_index_price(&self, symbol: Symbol, timestamp: u64) -> Result<IndexPrice, String> {
        let price = crate::domain::parse_decimal(&self.ctx.oracle_px)
            .map_err(|_| format!("Failed to parse oracle_px: {}", self.ctx.oracle_px))?;

        Ok(IndexPrice {
            exchange: Exchange::Hyperliquid,
            symbol,
            price,
            timestamp,
        })
    }
}

// ============================================================================
// 辅助函数
// ============================================================================

/// 计算下一个整点结算时间 (毫秒)
fn next_hourly_settle_time() -> u64 {
    let now = now_ms();
    let hour_ms = 3600 * 1000;
    let current_hour = now / hour_ms * hour_ms;
    current_hour + hour_ms
}

/// 计算数量精度（10 的负 `sz_decimals` 次方，恒为正）
pub fn size_step(sz_decimals: u32) -> crate::domain::PositiveDecimal {
    crate::domain::PositiveDecimal::new(Decimal::new(1, sz_decimals)).expect("10 的负幂恒为正")
}

// ============================================================================
// 账户状态（REST clearinghouseState）
// ============================================================================

/// Clearinghouse 状态（仓位与保证金摘要）
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClearinghouseState {
    pub asset_positions: Vec<AssetPositionWrapper>,
    /// 总账户保证金摘要（全仓 + 逐仓）；只有标准账户读它
    pub margin_summary: MarginSummary,
}

/// MarginSummary (保证金摘要)
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarginSummary {
    pub account_value: String,
}

/// AssetPosition 包装器
#[derive(Debug, Deserialize)]
pub struct AssetPositionWrapper {
    pub position: AssetPosition,
}

/// AssetPosition (单个仓位)
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetPosition {
    pub coin: String,
    /// 带符号的仓位大小 (负数为空头)
    pub szi: String,
}

impl AssetPosition {
    /// `symbol` 由调用方经 [`HlMarkets::symbol_of`] 得到
    pub fn to_position(&self, symbol: Symbol) -> Result<crate::domain::Position, String> {
        let size = crate::domain::parse_decimal(&self.szi)
            .map_err(|_| format!("Failed to parse szi: {}", self.szi))?;
        // 只取数量：均价与浮盈不进域模型（见 crate::domain::Position 的文档）——
        // 这也是 entry_px 的"空仓为 null / 在而解析不了"那套三态守卫可以整个删掉的原因
        Ok(crate::domain::Position {
            exchange: Exchange::Hyperliquid,
            symbol,
            size,
        })
    }
}

/// OrderUpdates 响应
#[derive(Debug, Deserialize)]
pub struct WsOrderUpdate {
    pub order: WsBasicOrder,
    pub status: String,
    #[serde(rename = "statusTimestamp")]
    pub status_timestamp: u64,
}

/// 基本订单信息
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WsBasicOrder {
    pub coin: String,
    pub side: String, // "A" (ask/sell) or "B" (bid/buy)
    /// 剩余未成交量
    pub sz: String,
    pub oid: u64,
    pub orig_sz: String,
    pub cloid: Option<String>,
}

impl WsOrderUpdate {
    /// `symbol` 由调用方经 [`HlMarkets::symbol_of`] 得到
    pub fn to_order_update(&self, symbol: Symbol) -> Result<crate::domain::OrderUpdate, String> {
        let orig_sz = crate::domain::parse_decimal(&self.order.orig_sz)
            .map_err(|_| format!("Failed to parse orig_sz: {}", self.order.orig_sz))?;
        let current_sz = crate::domain::parse_decimal(&self.order.sz)
            .map_err(|_| format!("Failed to parse sz: {}", self.order.sz))?;
        let filled_quantity = orig_sz - current_sz;

        // 本账户这一笔挂单的方向
        let side = parse_side(&self.order.side)?;

        let status = map_hyperliquid_order_status(&self.status, filled_quantity)?;

        Ok(crate::domain::OrderUpdate {
            order_id: self.order.oid.to_string(),
            client_order_id: self.order.cloid.clone(),
            exchange: Exchange::Hyperliquid,
            symbol,
            side,
            status,
            quantity: orig_sz,
            // origSz - sz：剩余量之外的就是累计成交（HL 是币本位）
            filled_quantity: filled_quantity.max(Decimal::ZERO),
        })
    }
}

/// Hyperliquid 订单状态映射。
///
/// HL 的终态有一大族（文档 orderStatus 表）：`canceled` 与 `xxxCanceled`（保证金不足、
/// 自成交、强平……撤单）、`scheduledCancel`，`rejected` 与 `xxxRejected`（价格精度、金额下限、
/// post-only 会吃单……拒单）。按文档的命名约定认后缀；别的（含本引擎不用的触发单 `triggered`）
/// 不猜（理由同 Binance 的 `to_order_update`），按解析失败处理。
fn map_hyperliquid_order_status(
    status: &str,
    filled: Quantity,
) -> Result<crate::domain::OrderStatus, String> {
    use crate::domain::OrderStatus;
    Ok(match status {
        "open" if filled > Decimal::ZERO => OrderStatus::PartiallyFilled,
        "open" => OrderStatus::Pending,
        "filled" => OrderStatus::Filled,
        "canceled" | "scheduledCancel" => OrderStatus::Cancelled,
        s if s.ends_with("Canceled") => OrderStatus::Cancelled,
        s if s == "rejected" || s.ends_with("Rejected") => OrderStatus::Rejected {
            reason: s.to_string(),
        },
        other => return Err(format!("Hyperliquid 订单状态未知: {other}")),
    })
}

// ============================================================================
// userFills 消息结构
// ============================================================================

/// userFills 响应包装器
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WsUserFills {
    pub user: String,
    pub fills: Vec<WsFill>,
    #[serde(default)]
    pub is_snapshot: bool,
}

/// 单个成交记录
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WsFill {
    pub coin: String,
    pub px: String,
    pub sz: String,
    pub side: String, // "B" (buy) or "A" (sell/ask)
    pub time: u64,
    pub oid: u64,
    #[serde(default)]
    pub cloid: Option<String>,
    pub fee: String,
    /// 手续费币种（永续成交是 USDC；返佣、现货等另论）
    pub fee_token: String,
    /// 强平信息对象——仅强平成交时存在 (HL userFills `liquidation` 字段)
    #[serde(default)]
    pub liquidation: Option<serde_json::Value>,
}

impl WsFill {
    /// `symbol` 由调用方经 [`HlMarkets::symbol_of`] 得到；手续费按 `feeToken` 如实记
    pub fn to_fill(&self, symbol: Symbol) -> Result<Fill, String> {
        let price = crate::domain::parse_decimal(&self.px)
            .map_err(|_| format!("Failed to parse fill px: {}", self.px))?;
        let size = crate::domain::parse_decimal(&self.sz)
            .map_err(|_| format!("Failed to parse fill sz: {}", self.sz))?;

        // 本账户这一笔成交的方向
        let side = parse_side(&self.side)?;

        let amount = f64::from_str(&self.fee)
            .map_err(|_| format!("Failed to parse fill fee: {}", self.fee))?;
        let fee = crate::domain::Money::fee_from_report(amount, Some(&self.fee_token))
            .map_err(|e| format!("HL {e}（订单 {}）", self.oid))?;

        // HL 强平成交带 `liquidation` 对象；HL 无独立 ADL 语义，故只区分 Liquidation
        let reason = if self.liquidation.is_some() {
            crate::domain::FillReason::Liquidation
        } else {
            crate::domain::FillReason::Normal
        };

        Ok(Fill {
            exchange: Exchange::Hyperliquid,
            symbol,
            side,
            price,
            size,
            client_order_id: self.cloid.clone(),
            order_id: self.oid.to_string(),
            timestamp: self.time,
            fee,
            reason,
        })
    }
}

#[cfg(test)]
mod clearinghouse_tests {
    use super::*;
    use crate::exchange::hyperliquid::markets::dex_of_coin;
    use rust_decimal_macros::dec;

    /// 取自 `POST /info {"type":"clearinghouseState","user":...,"dex":"xyz"}` 的**真实响应**
    /// （2026-08 实测股票永续 dex）。
    ///
    /// 钉死三个此前只能靠推断的口径：
    /// - **请求带 `dex` 时，`coin` 仍带 dex 前缀**（`xyz:NVDA`）—— 客户端按前缀判定仓位属于
    ///   哪个 dex（[`dex_of_coin`]）；若响应改成不带前缀，本测试会立刻失败，
    ///   而不是让 `fetch_positions` 静默过滤成空、把持仓基线变成全零
    /// - `szi` **带符号**（空头为负）
    /// - `leverage.rawUsd` 只在逐仓时出现，`cumFunding` / `time` 是本结构未声明的额外字段
    const REAL_CLEARINGHOUSE_STATE: &str = r#"{
      "marginSummary": {"accountValue":"24217.394304","totalNtlPos":"12100.48",
                        "totalRawUsd":"36317.874304","totalMarginUsed":"6385.392074"},
      "crossMarginSummary": {"accountValue":"17832.00223","totalNtlPos":"0.0",
                             "totalRawUsd":"17832.00223","totalMarginUsed":"0.0"},
      "crossMaintenanceMarginUsed": "0.0",
      "withdrawable": "17832.00223",
      "assetPositions": [
        {"type":"oneWay","position":{
          "coin":"xyz:NVDA","szi":"-56.0",
          "leverage":{"type":"isolated","value":2,"rawUsd":"18485.872074"},
          "entryPx":"217.057","positionValue":"12100.48","unrealizedPnl":"54.765972",
          "returnOnEquity":"0.0090110841","liquidationPx":"322.053520453",
          "marginUsed":"6385.392074","maxLeverage":20,
          "cumFunding":{"allTime":"-469.942577","sinceOpen":"-310.436405",
                        "sinceChange":"-27.857483"}}}
      ],
      "time": 1785923125277
    }"#;

    /// 文档里的整族撤单 / 拒单状态按后缀认成终态；别的不认识的按解析失败处理
    #[test]
    fn order_status_family() {
        use crate::domain::OrderStatus;
        assert_eq!(
            map_hyperliquid_order_status("open", dec!(0.0)).unwrap(),
            OrderStatus::Pending
        );
        assert_eq!(
            map_hyperliquid_order_status("open", dec!(1.0)).unwrap(),
            OrderStatus::PartiallyFilled
        );
        for s in [
            "canceled",
            "marginCanceled",
            "selfTradeCanceled",
            "scheduledCancel",
        ] {
            assert_eq!(
                map_hyperliquid_order_status(s, dec!(0.0)).unwrap(),
                OrderStatus::Cancelled,
                "{s}"
            );
        }
        for s in ["rejected", "badAloPxRejected", "perpMarginRejected"] {
            assert!(
                matches!(
                    map_hyperliquid_order_status(s, dec!(0.0)).unwrap(),
                    OrderStatus::Rejected { .. }
                ),
                "{s}"
            );
        }
        for s in ["triggered", "somethingNew", ""] {
            assert!(map_hyperliquid_order_status(s, dec!(0.0)).is_err(), "{s}");
        }
    }

    /// 整份响应能解析（任一必需字段缺失都会让持仓基线拿不到，故整体解析必须有测试守着）
    #[test]
    fn real_clearinghouse_state_parses() {
        let state: ClearinghouseState =
            serde_json::from_str(REAL_CLEARINGHOUSE_STATE).expect("解析真实 clearinghouseState");

        assert_eq!(state.margin_summary.account_value, "24217.394304");
        assert_eq!(state.asset_positions.len(), 1);
    }

    /// **Critical 防线**：请求带 dex 时 coin 仍带前缀，故按前缀归属 dex 成立。
    ///
    /// 若哪天响应改成不带前缀，过滤会把持仓全部滤掉 → `fetch_positions` 返回空 → 持仓基线
    /// 变成全零、之后由 Fill 累加且永不纠正。本测试就是那道防线。
    #[test]
    fn coin_keeps_dex_prefix_so_filtering_works() {
        let state: ClearinghouseState = serde_json::from_str(REAL_CLEARINGHOUSE_STATE).unwrap();
        let coin = &state.asset_positions[0].position.coin;

        assert_eq!(
            coin, "xyz:NVDA",
            "响应不再带 dex 前缀，fetch_positions 的过滤会把持仓滤空"
        );
        assert_eq!(dex_of_coin(coin), "xyz", "xyz dex 的持仓必须归到 xyz");
    }

    /// 空头的 `szi` 带负号，折算后必须保留方向
    #[test]
    fn short_position_keeps_negative_sign() {
        let state: ClearinghouseState = serde_json::from_str(REAL_CLEARINGHOUSE_STATE).unwrap();
        let position = state.asset_positions[0]
            .position
            .to_position("NVDA".into())
            .unwrap();

        assert_eq!(position.exchange, Exchange::Hyperliquid);
        assert_eq!(position.symbol, "NVDA");
        assert_eq!(position.size, dec!(-56.0), "got {}", position.size);
    }
}

#[cfg(test)]
mod trade_tests {
    use super::*;
    use rust_decimal_macros::dec;

    /// 品种目录：主站 BTC、xyz 的 NVDA
    fn m() -> HlMarkets {
        crate::exchange::hyperliquid::testkit::markets(&["", "xyz"])
    }

    fn trade(side: &str, px: &str, sz: &str, time: u64, hash: &str, users: [&str; 2]) -> WsTrade {
        WsTrade {
            coin: "BTC".to_string(),
            side: side.to_string(),
            px: px.to_string(),
            sz: sz.to_string(),
            time,
            hash: Some(hash.to_string()),
            users: Some(users.iter().map(|s| s.to_string()).collect()),
        }
    }

    const ZERO_HASH: &str = "0x0000000000000000000000000000000000000000000000000000000000000000";
    const TAKER: &str = "0xtaker";

    #[test]
    fn single_trade_parses_price_qty_and_taker_side() {
        // side=A 是主动卖 -> 买方为挂单方
        let t = trade(
            "A",
            "42219.9",
            "0.125",
            1630048897897,
            "0xabc",
            ["0xbuyer", TAKER],
        );
        let out = aggregate_trades(&[t], &m()).expect("aggregate");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].exchange, Exchange::Hyperliquid);
        assert_eq!(out[0].symbol, "BTC");
        assert_eq!(out[0].price, dec!(42219.9));
        assert_eq!(out[0].qty, dec!(0.125));
        assert!(out[0].is_buyer_maker);
        assert_eq!(out[0].timestamp, 1630048897897);
    }

    /// 线路上的编码实测为 `"B"`/`"A"`（官方文档写作 "Buy"/"Sell"，与实际不符）。
    /// 未观测到的形式不做预留容错，避免为想象中的输入写代码。
    #[test]
    fn trade_side_uses_b_a_encoding_only() {
        for (side, expect_buyer_maker) in [("B", false), ("A", true)] {
            let t = trade(side, "1.0", "1.0", 1, "0xabc", ["0xb", "0xs"]);
            let out = aggregate_trades(&[t], &m()).unwrap();
            assert_eq!(out[0].is_buyer_maker, expect_buyer_maker, "side={side}");
        }
        for side in ["Buy", "Sell", "buy", "sell"] {
            let t = trade(side, "1.0", "1.0", 1, "0xabc", ["0xb", "0xs"]);
            assert!(
                aggregate_trades(&[t], &m()).is_err(),
                "side={side} 应被拒绝"
            );
        }
    }

    /// coin 经品种目录换成内部 symbol（剥 dex 前缀）；目录外的 coin 报错
    #[test]
    fn trade_coin_maps_through_the_catalogue() {
        let mut t = trade("B", "1.0", "1.0", 1, "0xabc", ["0xb", "0xs"]);
        t.coin = "xyz:NVDA".to_string();
        assert_eq!(aggregate_trades(&[t], &m()).unwrap()[0].symbol, "NVDA");
        let mut foreign = trade("B", "1.0", "1.0", 1, "0xabc", ["0xb", "0xs"]);
        foreign.coin = "flx:NVDA".to_string();
        assert!(aggregate_trades(&[foreign], &m()).is_err());
    }

    /// 同一主动单在同一价位的多笔撮合合并为一条（aggTrade 口径）
    #[test]
    fn same_taker_order_same_price_is_merged() {
        let trades = vec![
            trade("A", "62508.0", "0.002", 100, "0xdeal", ["0xm1", TAKER]),
            trade("A", "62508.0", "0.005", 100, "0xdeal", ["0xm2", TAKER]),
            trade("A", "62508.0", "0.003", 120, "0xdeal", ["0xm3", TAKER]),
        ];
        let out = aggregate_trades(&trades, &m()).unwrap();
        assert_eq!(out.len(), 1, "同单同价应合并为一条");
        assert_eq!(out[0].qty, dec!(0.010), "数量应求和, got {}", out[0].qty);
        assert_eq!(out[0].timestamp, 120, "时间取该组最后一笔");
    }

    /// 同一主动单吃穿多档：不同价位必须各自成条（与 Binance aggTrade 一致）
    #[test]
    fn same_taker_order_different_prices_stay_separate() {
        let trades = vec![
            trade("A", "62508.0", "0.002", 100, "0xdeal", ["0xm1", TAKER]),
            trade("A", "62506.0", "0.004", 100, "0xdeal", ["0xm2", TAKER]),
        ];
        let out = aggregate_trades(&trades, &m()).unwrap();
        assert_eq!(out.len(), 2);
        // 保持首次出现顺序 -> 回测确定性
        assert_eq!(out[0].price, dec!(62508.0));
        assert_eq!(out[1].price, dec!(62506.0));
    }

    /// hash 为全 0（尚未上链）时退化为按主动方地址归集
    #[test]
    fn zero_hash_falls_back_to_taker_address() {
        let trades = vec![
            trade("A", "62503.0", "0.001", 100, ZERO_HASH, ["0xm1", TAKER]),
            trade("A", "62503.0", "0.002", 100, ZERO_HASH, ["0xm2", TAKER]),
            // 同价位但不同主动方 -> 不能合并
            trade("A", "62503.0", "0.004", 100, ZERO_HASH, ["0xm3", "0xother"]),
        ];
        let out = aggregate_trades(&trades, &m()).unwrap();
        assert_eq!(out.len(), 2, "不同主动方不得合并");
        assert_eq!(out[0].qty, dec!(0.003));
        assert_eq!(out[1].qty, dec!(0.004));
    }

    /// 方向相反不得合并（主动买与主动卖是两回事）
    #[test]
    fn opposite_taker_sides_are_not_merged() {
        let trades = vec![
            trade("B", "62503.0", "0.001", 100, ZERO_HASH, [TAKER, "0xm1"]),
            trade("A", "62503.0", "0.002", 100, ZERO_HASH, ["0xm2", TAKER]),
        ];
        assert_eq!(aggregate_trades(&trades, &m()).unwrap().len(), 2);
    }

    /// 缺 users 字段时不按地址归集，但仍按 hash 归集（不 panic、不误合并）
    #[test]
    fn missing_users_still_aggregates_by_hash() {
        let mut a = trade("A", "1.0", "1.0", 1, "0xdeal", ["0xb", "0xs"]);
        let mut b = trade("A", "1.0", "2.0", 1, "0xdeal", ["0xb", "0xs"]);
        a.users = None;
        b.users = None;
        let out = aggregate_trades(&[a, b], &m()).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].qty, dec!(3.0));
    }
}

#[cfg(test)]
mod bbo_tests {
    use super::*;
    use rust_decimal_macros::dec;

    /// **单边盘口是合法状态**：bid/ask 为 null 返回 Ok(None) 跳过，绝不能按坏报文
    /// 返回 Err —— 那会经 fail-fast 链路 kill actor、级联整机停机
    #[test]
    fn one_sided_book_is_skipped_not_fatal() {
        let raw = r#"{"coin":"ETH","time":1,"bbo":[null,{"px":"100.0","sz":"1.0","n":1}]}"#;
        let d: WsBbo = serde_json::from_str(raw).unwrap();
        assert!(d.to_bbo("ETH".into()).expect("不是错误").is_none());

        let raw = r#"{"coin":"ETH","time":1,"bbo":[{"px":"99.0","sz":"1.0","n":1},null]}"#;
        let d: WsBbo = serde_json::from_str(raw).unwrap();
        assert!(d.to_bbo("ETH".into()).expect("不是错误").is_none());

        let raw = r#"{"coin":"ETH","time":1,"bbo":[{"px":"99.0","sz":"1.0","n":1},{"px":"100.0","sz":"1.0","n":1}]}"#;
        let d: WsBbo = serde_json::from_str(raw).unwrap();
        let bbo = d.to_bbo("ETH".into()).expect("不是错误").expect("双边盘口");
        assert_eq!(bbo.bid_price, dec!(99.0));
        assert_eq!(bbo.ask_price, dec!(100.0));
    }
}
