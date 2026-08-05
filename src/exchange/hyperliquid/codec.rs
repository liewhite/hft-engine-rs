//! Hyperliquid 数据编解码
//!
//! 解析 Hyperliquid REST API 和 WebSocket 消息

#![allow(dead_code)]

use super::from_hyperliquid;
use crate::domain::{
    now_ms, Exchange, Fill, FundingRate, IndexPrice, MarkPrice, MarketTrade, Side, BBO,
};
use indexmap::IndexMap;
use serde::Deserialize;
use std::str::FromStr;

/// Hyperliquid 全站方向编码：`"B"` = bid/买、`"A"` = ask/卖。
///
/// 三处推送共用这套编码，但**语义各不相同**：订单/成交推送里是"本账户这一笔的方向"，
/// trades 里是"主动方 (taker) 的方向"。故此处只统一**编码**解析，语义由各调用方自行表达
/// —— 合并成一个 "parse_taker_side" 会把两种含义混为一谈。
fn parse_side(raw: &str) -> Result<Side, String> {
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
}

/// 单个资产信息
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetInfo {
    /// 币种名 (e.g., "BTC", "ETH")
    pub name: String,
    /// 数量小数位数
    pub sz_decimals: i32,
    /// 最大杠杆
    pub max_leverage: u32,
    /// 是否已下架
    #[serde(default)]
    pub is_delisted: bool,
    /// 保证金模式 (None = 默认允许全仓, "strictIsolated" = 仅逐仓, "noCross" = 不支持全仓)
    pub margin_mode: Option<String>,
}

impl AssetInfo {
    /// 是否支持全仓保证金模式
    pub fn supports_cross_margin(&self) -> bool {
        match self.margin_mode.as_deref() {
            None => true, // 默认允许全仓
            Some("strictIsolated") | Some("noCross") => false,
            Some(_) => true, // 其他模式默认允许
        }
    }
}

/// 资产上下文 (包含资金费率等实时数据)
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetCtx {
    /// 当前资金费率
    pub funding: String,
    /// 持仓量
    pub open_interest: String,
    /// 标记价格
    pub mark_px: String,
    /// 中间价
    pub mid_px: String,
    /// oracle 价格
    pub oracle_px: String,
    /// 冲击价格 [bid_impact, ask_impact]
    pub impact_pxs: Option<Vec<String>>,
}

// ============================================================================
// WebSocket 消息结构
// ============================================================================

/// WebSocket 订阅响应
#[derive(Debug, Deserialize)]
pub struct WsSubscriptionResponse {
    pub channel: String,
    pub data: serde_json::Value,
}

/// AllMids 数据 (所有中间价)
#[derive(Debug, Deserialize)]
pub struct AllMids {
    pub mids: std::collections::HashMap<String, String>,
}

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
    /// 订单数量
    pub n: u32,
}

impl WsBbo {
    pub fn to_bbo(&self) -> Result<BBO, String> {
        let symbol = from_hyperliquid(&self.coin);

        let bid = self.bbo[0].as_ref()
            .ok_or("BBO bid is null")?;
        let ask = self.bbo[1].as_ref()
            .ok_or("BBO ask is null")?;

        let bid_price = f64::from_str(&bid.px)
            .map_err(|_| format!("Failed to parse bid price: {}", bid.px))?;
        let bid_qty = f64::from_str(&bid.sz)
            .map_err(|_| format!("Failed to parse bid size: {}", bid.sz))?;
        let ask_price = f64::from_str(&ask.px)
            .map_err(|_| format!("Failed to parse ask price: {}", ask.px))?;
        let ask_qty = f64::from_str(&ask.sz)
            .map_err(|_| format!("Failed to parse ask size: {}", ask.sz))?;

        Ok(BBO {
            exchange: Exchange::Hyperliquid,
            symbol,
            bid_price,
            bid_qty,
            ask_price,
            ask_qty,
            timestamp: self.time,
        })
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
pub fn aggregate_trades(trades: &[WsTrade]) -> Result<Vec<MarketTrade>, String> {
    let mut out: IndexMap<(Option<String>, Option<String>, u64, bool), MarketTrade> =
        IndexMap::new();

    for t in trades {
        let taker_side = parse_side(&t.side)?;
        // 主动卖 (A) 时买方为挂单方
        let is_buyer_maker = taker_side == Side::Short;
        let price = f64::from_str(&t.px)
            .map_err(|_| format!("Failed to parse trade price: {}", t.px))?;
        let qty = f64::from_str(&t.sz)
            .map_err(|_| format!("Failed to parse trade size: {}", t.sz))?;

        // 价格按位模式作键：f64 不可哈希，且此处只需"完全相同"的语义
        let key = (
            t.taker_order_hash().map(str::to_string),
            t.taker(taker_side).map(str::to_string),
            price.to_bits(),
            is_buyer_maker,
        );
        out.entry(key)
            .and_modify(|agg| {
                agg.qty += qty;
                agg.timestamp = agg.timestamp.max(t.time);
            })
            .or_insert_with(|| MarketTrade {
                exchange: Exchange::Hyperliquid,
                symbol: from_hyperliquid(&t.coin),
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
    pub fn to_funding_rate(&self, timestamp: u64) -> Result<FundingRate, String> {
        let symbol = from_hyperliquid(&self.coin);
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
    pub fn to_mark_price(&self, timestamp: u64) -> Result<MarkPrice, String> {
        let symbol = from_hyperliquid(&self.coin);
        let price = f64::from_str(&self.ctx.mark_px)
            .map_err(|_| format!("Failed to parse mark_px: {}", self.ctx.mark_px))?;

        Ok(MarkPrice {
            exchange: Exchange::Hyperliquid,
            symbol,
            price,
            timestamp,
        })
    }

    /// 转换为 IndexPrice (使用 oracle_px 作为指数价格)
    pub fn to_index_price(&self, timestamp: u64) -> Result<IndexPrice, String> {
        let symbol = from_hyperliquid(&self.coin);
        let price = f64::from_str(&self.ctx.oracle_px)
            .map_err(|_| format!("Failed to parse oracle_px: {}", self.ctx.oracle_px))?;

        Ok(IndexPrice {
            exchange: Exchange::Hyperliquid,
            symbol,
            price,
            timestamp,
        })
    }

    /// 获取 symbol
    pub fn symbol(&self) -> crate::domain::Symbol {
        from_hyperliquid(&self.coin)
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

/// 计算数量精度
pub fn size_step(sz_decimals: i32) -> f64 {
    10f64.powi(-sz_decimals)
}

// ============================================================================
// WebSocket 账户订阅消息结构
// ============================================================================

/// WebData3 响应 (账户状态)
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WsWebData3 {
    pub clearinghouse_state: Option<ClearinghouseState>,
}

/// Clearinghouse 状态 (仓位和余额)
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClearinghouseState {
    pub asset_positions: Vec<AssetPositionWrapper>,
    /// 总账户保证金摘要（全仓 + 逐仓）
    pub margin_summary: MarginSummary,
    /// 全仓部分的保证金摘要
    pub cross_margin_summary: MarginSummary,
    pub withdrawable: String,
}

/// MarginSummary (保证金摘要)
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarginSummary {
    pub account_value: String,
    pub total_ntl_pos: String,
    pub total_raw_usd: String,
    pub total_margin_used: String,
}

/// AssetPosition 包装器
#[derive(Debug, Deserialize)]
pub struct AssetPositionWrapper {
    pub position: AssetPosition,
    #[serde(rename = "type")]
    pub position_type: String,
}

/// AssetPosition (单个仓位)
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetPosition {
    pub coin: String,
    /// 带符号的仓位大小 (负数为空头)
    pub szi: String,
    pub entry_px: Option<String>,
    pub leverage: PositionLeverage,
    pub liquidation_px: Option<String>,
    pub unrealized_pnl: String,
    pub margin_used: String,
    pub position_value: String,
    pub return_on_equity: String,
    pub max_leverage: u32,
}

/// 杠杆信息
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionLeverage {
    #[serde(rename = "type")]
    pub leverage_type: String, // "cross" or "isolated"
    pub value: u32,
    pub raw_usd: Option<String>,
}

impl AssetPosition {
    pub fn to_position(&self) -> Result<crate::domain::Position, String> {
        let symbol = from_hyperliquid(&self.coin);
        let size = f64::from_str(&self.szi)
            .map_err(|_| format!("Failed to parse szi: {}", self.szi))?;
        // entry_px 在空仓 (size=0) 时可能为 None，此时使用 0.0
        let entry_price = match self.entry_px.as_deref() {
            Some(p) => f64::from_str(p).unwrap_or_else(|_| {
                tracing::warn!(coin = %self.coin, entry_px = %p, "Failed to parse entry_px, defaulting to 0.0");
                0.0
            }),
            None => 0.0,
        };
        let unrealized_pnl = f64::from_str(&self.unrealized_pnl)
            .map_err(|_| format!("Failed to parse unrealized_pnl: {}", self.unrealized_pnl))?;

        Ok(crate::domain::Position {
            exchange: Exchange::Hyperliquid,
            symbol,
            size,
            entry_price,
            unrealized_pnl,
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
    pub limit_px: String,
    pub sz: String,
    pub oid: u64,
    pub timestamp: u64,
    pub orig_sz: String,
    pub cloid: Option<String>,
}

impl WsOrderUpdate {
    pub fn to_order_update(&self) -> Result<crate::domain::OrderUpdate, String> {
        let symbol = from_hyperliquid(&self.order.coin);
        let orig_sz = f64::from_str(&self.order.orig_sz)
            .map_err(|_| format!("Failed to parse orig_sz: {}", self.order.orig_sz))?;
        let current_sz = f64::from_str(&self.order.sz)
            .map_err(|_| format!("Failed to parse sz: {}", self.order.sz))?;
        let filled_quantity = orig_sz - current_sz;

        // 本账户这一笔挂单的方向
        let side = parse_side(&self.order.side)?;

        let status = map_hyperliquid_order_status(&self.status, filled_quantity);

        // Hyperliquid 不直接提供本次成交量，使用 filled_quantity
        // 对于 IOC 订单（一次性成交），这等于实际成交量
        let price = f64::from_str(&self.order.limit_px)
            .map_err(|_| format!("Failed to parse limit_px: {}", self.order.limit_px))?;

        Ok(crate::domain::OrderUpdate {
            order_id: self.order.oid.to_string(),
            client_order_id: self.order.cloid.clone(),
            exchange: Exchange::Hyperliquid,
            symbol,
            side,
            status,
            price,
            reduce_only: false, // HL WsBasicOrder 不含 reduceOnly 字段
            quantity: orig_sz,
            filled_quantity,
            fill_sz: filled_quantity,
            timestamp: self.status_timestamp,
        })
    }
}

/// Hyperliquid 订单状态映射
fn map_hyperliquid_order_status(status: &str, filled: f64) -> crate::domain::OrderStatus {
    match status {
        "open" => {
            if filled > 0.0 {
                crate::domain::OrderStatus::PartiallyFilled { filled }
            } else {
                crate::domain::OrderStatus::Pending
            }
        }
        "filled" => crate::domain::OrderStatus::Filled,
        "canceled" | "cancelled" => crate::domain::OrderStatus::Cancelled,
        "rejected" => crate::domain::OrderStatus::Rejected {
            reason: "Order rejected".to_string(),
        },
        other => crate::domain::OrderStatus::Rejected {
            reason: format!("Unknown status: {}", other),
        },
    }
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
    pub closed_pnl: String,
    /// 强平信息对象——仅强平成交时存在 (HL userFills `liquidation` 字段)
    #[serde(default)]
    pub liquidation: Option<serde_json::Value>,
}

impl WsFill {
    pub fn to_fill(&self) -> Result<Fill, String> {
        let symbol = from_hyperliquid(&self.coin);
        let price = f64::from_str(&self.px)
            .map_err(|_| format!("Failed to parse fill px: {}", self.px))?;
        let size = f64::from_str(&self.sz)
            .map_err(|_| format!("Failed to parse fill sz: {}", self.sz))?;

        // 本账户这一笔成交的方向
        let side = parse_side(&self.side)?;

        let fee = f64::from_str(&self.fee)
            .map_err(|_| format!("Failed to parse fill fee: {}", self.fee))?;

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
    use crate::exchange::hyperliquid::symbol::belongs_to_dex;

    /// 取自 `POST /info {"type":"clearinghouseState","user":...,"dex":"xyz"}` 的**真实响应**
    /// （2026-08 实测股票永续 dex）。
    ///
    /// 钉死三个此前只能靠推断的口径：
    /// - **请求带 `dex` 时，`coin` 仍带 dex 前缀**（`xyz:NVDA`）—— 故客户端的
    ///   [`belongs_to_dex`] 过滤是必要且正确的；若响应改成不带前缀，本测试会立刻失败，
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

    /// 整份响应能解析（任一必需字段缺失都会让持仓基线拿不到，故整体解析必须有测试守着）
    #[test]
    fn real_clearinghouse_state_parses() {
        let state: ClearinghouseState =
            serde_json::from_str(REAL_CLEARINGHOUSE_STATE).expect("解析真实 clearinghouseState");

        assert_eq!(state.margin_summary.account_value, "24217.394304");
        assert_eq!(state.margin_summary.total_ntl_pos, "12100.48");
        assert_eq!(state.withdrawable, "17832.00223");
        assert_eq!(state.asset_positions.len(), 1);
    }

    /// **Critical 防线**：请求带 dex 时 coin 仍带前缀，故 `belongs_to_dex` 过滤成立。
    ///
    /// 若哪天响应改成不带前缀，过滤会把持仓全部滤掉 → `fetch_positions` 返回空 → 持仓基线
    /// 变成全零、之后由 Fill 累加且永不纠正。本测试就是那道防线。
    #[test]
    fn coin_keeps_dex_prefix_so_filtering_works() {
        let state: ClearinghouseState =
            serde_json::from_str(REAL_CLEARINGHOUSE_STATE).unwrap();
        let coin = &state.asset_positions[0].position.coin;

        assert_eq!(coin, "xyz:NVDA", "响应不再带 dex 前缀，fetch_positions 的过滤会把持仓滤空");
        assert!(belongs_to_dex(coin, "xyz"), "xyz dex 的持仓必须能通过过滤");
        assert!(!belongs_to_dex(coin, ""), "带前缀的 coin 不属于默认 perp DEX");
    }

    /// 空头的 `szi` 带负号，折算后必须保留方向
    #[test]
    fn short_position_keeps_negative_sign() {
        let state: ClearinghouseState =
            serde_json::from_str(REAL_CLEARINGHOUSE_STATE).unwrap();
        let position = state.asset_positions[0].position.to_position().unwrap();

        assert_eq!(position.exchange, Exchange::Hyperliquid);
        // dex 前缀由 from_hyperliquid 剥掉，得到内部 symbol
        assert_eq!(position.symbol, "NVDA");
        assert!((position.size - (-56.0)).abs() < 1e-9, "got {}", position.size);
        assert!((position.entry_price - 217.057).abs() < 1e-9);
        assert!((position.unrealized_pnl - 54.765972).abs() < 1e-9);
    }
}

#[cfg(test)]
mod trade_tests {
    use super::*;

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
        let t = trade("A", "42219.9", "0.125", 1630048897897, "0xabc", ["0xbuyer", TAKER]);
        let out = aggregate_trades(&[t]).expect("aggregate");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].exchange, Exchange::Hyperliquid);
        assert_eq!(out[0].symbol, "BTC");
        assert_eq!(out[0].price, 42219.9);
        assert_eq!(out[0].qty, 0.125);
        assert!(out[0].is_buyer_maker);
        assert_eq!(out[0].timestamp, 1630048897897);
    }

    /// 线路上的编码实测为 `"B"`/`"A"`（官方文档写作 "Buy"/"Sell"，与实际不符）。
    /// 未观测到的形式不做预留容错，避免为想象中的输入写代码。
    #[test]
    fn trade_side_uses_b_a_encoding_only() {
        for (side, expect_buyer_maker) in [("B", false), ("A", true)] {
            let t = trade(side, "1.0", "1.0", 1, "0xabc", ["0xb", "0xs"]);
            let out = aggregate_trades(&[t]).unwrap();
            assert_eq!(out[0].is_buyer_maker, expect_buyer_maker, "side={side}");
        }
        for side in ["Buy", "Sell", "buy", "sell"] {
            let t = trade(side, "1.0", "1.0", 1, "0xabc", ["0xb", "0xs"]);
            assert!(aggregate_trades(&[t]).is_err(), "side={side} 应被拒绝");
        }
    }

    /// dex 前缀应被剥离为 base symbol
    #[test]
    fn trade_strips_dex_prefix() {
        let mut t = trade("B", "1.0", "1.0", 1, "0xabc", ["0xb", "0xs"]);
        t.coin = "xyz:NVDA".to_string();
        assert_eq!(aggregate_trades(&[t]).unwrap()[0].symbol, "NVDA");
    }

    /// 同一主动单在同一价位的多笔撮合合并为一条（aggTrade 口径）
    #[test]
    fn same_taker_order_same_price_is_merged() {
        let trades = vec![
            trade("A", "62508.0", "0.002", 100, "0xdeal", ["0xm1", TAKER]),
            trade("A", "62508.0", "0.005", 100, "0xdeal", ["0xm2", TAKER]),
            trade("A", "62508.0", "0.003", 120, "0xdeal", ["0xm3", TAKER]),
        ];
        let out = aggregate_trades(&trades).unwrap();
        assert_eq!(out.len(), 1, "同单同价应合并为一条");
        assert!((out[0].qty - 0.010).abs() < 1e-12, "数量应求和, got {}", out[0].qty);
        assert_eq!(out[0].timestamp, 120, "时间取该组最后一笔");
    }

    /// 同一主动单吃穿多档：不同价位必须各自成条（与 Binance aggTrade 一致）
    #[test]
    fn same_taker_order_different_prices_stay_separate() {
        let trades = vec![
            trade("A", "62508.0", "0.002", 100, "0xdeal", ["0xm1", TAKER]),
            trade("A", "62506.0", "0.004", 100, "0xdeal", ["0xm2", TAKER]),
        ];
        let out = aggregate_trades(&trades).unwrap();
        assert_eq!(out.len(), 2);
        // 保持首次出现顺序 -> 回测确定性
        assert_eq!(out[0].price, 62508.0);
        assert_eq!(out[1].price, 62506.0);
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
        let out = aggregate_trades(&trades).unwrap();
        assert_eq!(out.len(), 2, "不同主动方不得合并");
        assert!((out[0].qty - 0.003).abs() < 1e-12);
        assert!((out[1].qty - 0.004).abs() < 1e-12);
    }

    /// 方向相反不得合并（主动买与主动卖是两回事）
    #[test]
    fn opposite_taker_sides_are_not_merged() {
        let trades = vec![
            trade("B", "62503.0", "0.001", 100, ZERO_HASH, [TAKER, "0xm1"]),
            trade("A", "62503.0", "0.002", 100, ZERO_HASH, ["0xm2", TAKER]),
        ];
        assert_eq!(aggregate_trades(&trades).unwrap().len(), 2);
    }

    /// 缺 users 字段时不按地址归集，但仍按 hash 归集（不 panic、不误合并）
    #[test]
    fn missing_users_still_aggregates_by_hash() {
        let mut a = trade("A", "1.0", "1.0", 1, "0xdeal", ["0xb", "0xs"]);
        let mut b = trade("A", "1.0", "2.0", 1, "0xdeal", ["0xb", "0xs"]);
        a.users = None;
        b.users = None;
        let out = aggregate_trades(&[a, b]).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].qty, 3.0);
    }
}
