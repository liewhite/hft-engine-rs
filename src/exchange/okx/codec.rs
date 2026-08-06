use super::{from_okx, from_okx_index};
use crate::domain::{Candle, CandleInterval, Exchange, Fill, FundingRate, Greeks, IndexPrice, MarkPrice, MarketTrade, OrderStatus, OrderUpdate, Position, Side, Symbol, SymbolMeta, now_ms, BBO};
use std::collections::HashMap;
use serde::Deserialize;
use std::str::FromStr;

/// 由 OKX `instId` 解析出内部 symbol 并取其元数据。
///
/// 返回 `None` 表示 instId 无法识别或该 symbol 未配置 —— 调用方应跳过该条并告警，
/// **绝不能**退化为"不折算直接下发"（那会把张数当币数，见 [`crate::domain::Quantity`]）。
pub(crate) fn resolve_meta<'a>(
    inst_id: &str,
    metas: &'a HashMap<Symbol, SymbolMeta>,
) -> Option<&'a SymbolMeta> {
    let symbol = from_okx(inst_id)?;
    metas.get(&symbol)
}

/// OKX 方向编码：`"buy"` / `"sell"`。
///
/// 各推送共用这套编码但**语义不同**：订单/成交推送里是"本账户这一笔的方向"，trades 里是
/// "主动方 (taker) 的方向"。此处只统一**编码**解析，语义由各调用方自行表达。
fn parse_side(raw: &str) -> Result<Side, String> {
    match raw {
        "buy" => Ok(Side::Long),
        "sell" => Ok(Side::Short),
        other => Err(format!("Unknown OKX side: {}", other)),
    }
}

/// WebSocket 推送通用格式
#[derive(Debug, Deserialize)]
pub struct WsPush<T> {
    pub arg: WsArg,
    pub data: Vec<T>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WsArg {
    #[allow(dead_code)]
    pub channel: String,
    pub inst_id: Option<String>,
    #[allow(dead_code)]
    pub inst_type: Option<String>,
}

/// Funding Rate 数据
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FundingRateData {
    pub inst_id: String,
    #[allow(dead_code)]
    pub inst_type: String,
    pub funding_rate: String,
    #[allow(dead_code)]
    pub next_funding_rate: Option<String>,
    #[allow(dead_code)]
    pub funding_time: String,
    // pub next_funding_time: String,
}

impl FundingRateData {
    /// 转换为 FundingRate
    /// timestamp: 数据时间戳（毫秒）
    pub fn to_funding_rate(&self, timestamp: u64) -> Result<FundingRate, String> {
        let symbol = from_okx(&self.inst_id)
            .ok_or_else(|| format!("Unknown OKX symbol: {}", self.inst_id))?;
        let rate = f64::from_str(&self.funding_rate)
            .map_err(|_| format!("Failed to parse funding rate: {}", self.funding_rate))?;
        // funding_time: 下次收取时间
        let next_settle_ms: u64 = self.funding_time.parse()
            .map_err(|_| format!("Failed to parse funding time: {}", self.funding_time))?;

        Ok(FundingRate {
            exchange: Exchange::OKX,
            symbol,
            rate,
            next_settle_time: next_settle_ms,
            timestamp,
        })
    }
}

/// BBO 数据 (bbo-tbt channel)
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BboData {
    pub asks: Vec<Vec<String>>,
    pub bids: Vec<Vec<String>>,
    pub ts: String,
    #[allow(dead_code)]
    pub seq_id: Option<i64>,
}

impl BboData {
    /// `meta` 既提供 symbol，也提供张->币折算 —— 盘口挂单量 `sz` 与私有流一样是**张数**，
    /// 必须折算后再进 domain（见 [`crate::domain::Quantity`] 的不变量）。
    ///
    /// 返回 `Ok(None)` = **单边盘口**（bids 或 asks 为空）：流动性稀薄品种上的合法市场
    /// 状态，不是坏报文 —— 调用方跳过该条即可（domain 的 BBO 要求双边齐）。此前按解析
    /// 错误返回 Err，会经 fail-fast 链路 kill actor、级联整机停机。`Err` 只留给真正的
    /// 报文损坏（字段解析失败）。
    pub fn to_bbo(&self, meta: &SymbolMeta) -> Result<Option<BBO>, String> {
        let (Some(ask), Some(bid)) = (self.asks.first(), self.bids.first()) else {
            return Ok(None); // 单边盘口
        };
        if ask.len() < 2 {
            return Err(format!("BBO ask data incomplete: {:?}", ask));
        }
        let ask_price = f64::from_str(&ask[0])
            .map_err(|_| format!("Failed to parse ask price: {}", ask[0]))?;
        let ask_qty = f64::from_str(&ask[1])
            .map_err(|_| format!("Failed to parse ask qty: {}", ask[1]))?;

        if bid.len() < 2 {
            return Err(format!("BBO bid data incomplete: {:?}", bid));
        }
        let bid_price = f64::from_str(&bid[0])
            .map_err(|_| format!("Failed to parse bid price: {}", bid[0]))?;
        let bid_qty = f64::from_str(&bid[1])
            .map_err(|_| format!("Failed to parse bid qty: {}", bid[1]))?;

        let timestamp = self.ts.parse::<u64>()
            .map_err(|_| format!("Failed to parse timestamp: {}", self.ts))?;

        Ok(Some(BBO {
            exchange: Exchange::OKX,
            symbol: meta.symbol.clone(),
            bid_price,
            bid_qty: meta.qty_to_coin(bid_qty),
            ask_price,
            ask_qty: meta.qty_to_coin(ask_qty),
            timestamp,
        }))
    }
}

/// 成交数据 (trades channel)
///
/// 官方字段：`instId`、`tradeId`、`px` 成交价、`sz` 成交量、`side` **主动方**方向
/// (`buy`/`sell`)、`ts` 成交时间 (毫秒字符串)。其余字段 (`count`/`source` 等) 由 serde 忽略。
///
/// **线路上的 `sz` 单位是合约张数** (SWAP/FUTURES)；折算为币本位在本方法内完成，
/// 由签名强制要求 [`SymbolMeta`]（见 [`crate::domain::Quantity`] 的不变量）。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TradeData {
    #[allow(dead_code)]
    pub trade_id: String,
    pub px: String,
    pub sz: String,
    pub side: String,
    pub ts: String,
}

impl TradeData {
    /// 转为公共成交印记，数量已折算为币本位。
    pub fn to_market_trade(&self, meta: &SymbolMeta) -> Result<MarketTrade, String> {
        let price = f64::from_str(&self.px)
            .map_err(|_| format!("Failed to parse trade price: {}", self.px))?;
        let qty = f64::from_str(&self.sz)
            .map_err(|_| format!("Failed to parse trade qty: {}", self.sz))?;
        let timestamp = self
            .ts
            .parse::<u64>()
            .map_err(|_| format!("Failed to parse trade timestamp: {}", self.ts))?;
        // side 是**主动方**方向：主动卖时买方为挂单方
        let is_buyer_maker = parse_side(&self.side)? == Side::Short;

        Ok(MarketTrade {
            exchange: Exchange::OKX,
            symbol: meta.symbol.clone(),
            price,
            qty: meta.qty_to_coin(qty),
            is_buyer_maker,
            timestamp,
        })
    }
}

/// Mark Price 数据 (mark-price channel)
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkPriceData {
    pub inst_id: String,
    #[allow(dead_code)]
    pub inst_type: String,
    pub mark_px: String,
    pub ts: String,
}

impl MarkPriceData {
    pub fn to_mark_price(&self) -> Result<MarkPrice, String> {
        let symbol = from_okx(&self.inst_id)
            .ok_or_else(|| format!("Unknown OKX symbol: {}", self.inst_id))?;
        let price = f64::from_str(&self.mark_px)
            .map_err(|_| format!("Failed to parse mark price: {}", self.mark_px))?;
        let timestamp = self.ts.parse::<u64>()
            .map_err(|_| format!("Failed to parse timestamp: {}", self.ts))?;

        Ok(MarkPrice {
            exchange: Exchange::OKX,
            symbol,
            price,
            timestamp,
        })
    }
}

/// Index Ticker 数据 (index-tickers channel)
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexTickerData {
    pub inst_id: String,
    pub idx_px: String,
    pub ts: String,
}

impl IndexTickerData {
    pub fn to_index_price(&self) -> Result<IndexPrice, String> {
        // index-tickers 返回 BTC-USDT 格式，使用 from_okx_index 解析
        let symbol = from_okx_index(&self.inst_id)
            .ok_or_else(|| format!("Unknown OKX index symbol: {}", self.inst_id))?;
        let price = f64::from_str(&self.idx_px)
            .map_err(|_| format!("Failed to parse index price: {}", self.idx_px))?;
        let timestamp = self.ts.parse::<u64>()
            .map_err(|_| format!("Failed to parse timestamp: {}", self.ts))?;

        Ok(IndexPrice {
            exchange: Exchange::OKX,
            symbol,
            price,
            timestamp,
        })
    }
}

/// 单向净持仓模式的 `posSide` 取值。
///
/// 本项目**只支持单向净持仓**：双向（long/short 分列）模式下 `pos` 恒为正数、方向靠
/// `posSide` 表达，按净持仓口径解析会得到**错误的符号**。这属于账户模式配置错误，必须在
/// 解析处就拒绝——猜错方向的代价是策略朝反方向加仓。
const POS_SIDE_NET: &str = "net";

/// Position 数据（`GET /api/v5/account/positions` 与旧私有 WS `positions` 频道同构）
///
/// 只声明所需字段，其余（`instType` / `lever` / `mgnMode` 等）由 serde 忽略 —— 声明了却不读
/// 的字段一旦被交易所省略就会让整条解析失败，白担风险。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionData {
    pub inst_id: String,
    /// 持仓方向，见 [`POS_SIDE_NET`]
    pub pos_side: String,
    pub pos: String,
}

impl PositionData {
    /// 转为 domain 持仓。`pos` 是**张数**，折算为币本位由 `contract_size` 完成
    /// （见 [`crate::domain::Quantity`]）。
    ///
    /// 只取**数量**：均价与浮盈不进域模型（见 [`Position`] 的文档）。
    ///
    /// 这让"空仓时 OKX 用空串表示数值字段"那套三态守卫整个消失 —— 空串只可能出现在
    /// `pos` 上，而空仓的数量就是 0，如实且无歧义。
    pub fn to_position(&self, symbol: Symbol, contract_size: f64) -> Result<Position, String> {
        if self.pos_side != POS_SIDE_NET {
            return Err(format!(
                "OKX {} 处于双向持仓模式 (posSide={})，本项目仅支持单向净持仓；\
                 请在 OKX 账户设置中改为「单向持仓」后重启",
                self.inst_id, self.pos_side
            ));
        }
        // 空仓时 pos 为空串 —— 那就是 0 张，如实解析；非空则必须解析成功
        let contracts = if self.pos.is_empty() {
            0.0
        } else {
            f64::from_str(&self.pos).map_err(|_| format!("Failed to parse pos: {}", self.pos))?
        };
        Ok(Position {
            exchange: Exchange::OKX,
            symbol,
            // 正数多头，负数空头；张 -> 币
            size: contracts * contract_size,
        })
    }
}

/// Account 数据
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountData {
    pub u_time: String,
    /// 账户总权益 (USDT)
    pub total_eq: String,
    /// 账户总持仓名义价值 (USD)
    pub notional_usd: String,
    pub details: Vec<AccountDetail>,
}

impl AccountData {
    pub fn to_equity(&self) -> Result<f64, String> {
        f64::from_str(&self.total_eq)
            .map_err(|_| format!("Failed to parse OKX total equity: {}", self.total_eq))
    }

    pub fn to_notional(&self) -> Result<f64, String> {
        f64::from_str(&self.notional_usd)
            .map_err(|_| format!("Failed to parse OKX notionalUsd: {}", self.notional_usd))
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountDetail {
    pub ccy: String,
    #[allow(dead_code)]
    pub eq: String,
    #[allow(dead_code)]
    pub avail_eq: String,
    #[allow(dead_code)]
    pub avail_bal: String,
    #[allow(dead_code)]
    pub frozen_bal: String,
    /// 币种现金余额 (现货持有量)
    pub cash_bal: String,
}

/// Order 推送数据
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderPushData {
    pub inst_id: String,
    pub ord_id: String,
    pub cl_ord_id: Option<String>,
    pub side: String, // "buy" or "sell"
    pub state: String,
    /// 成交类别: normal / twap / adl / full_liquidation / partial_liquidation / delivery / ddh
    #[serde(default)]
    pub category: Option<String>,
    /// 订单总数量 (张)
    pub sz: String,
    /// 本次成交数量 (张)
    pub fill_sz: String,
    /// 本次成交价格
    pub fill_px: String,
    /// **本次成交**的手续费（OKX 文档：last filled fee）。无成交的推送里没有该字段。
    ///
    /// 注意不要用 `fee` —— 那是该订单**累计**的手续费/返佣，用它当单笔会让分批成交的
    /// 手续费被反复累加（见 `to_fill` 的说明）。
    #[serde(default)]
    pub fill_fee: Option<String>,
}

impl OrderPushData {
    /// 全部数量字段折算为币本位，折算由签名强制（见 [`crate::domain::Quantity`]）。
    pub fn to_order_update(&self, meta: &SymbolMeta) -> Result<OrderUpdate, String> {
        // market order px 为空字符串，此时 price 为 0.0
        // 线路上三个数量字段都是张数，此处一次性折算为币本位
        let sz = meta.qty_to_coin(
            f64::from_str(&self.sz).map_err(|_| format!("Failed to parse sz: {}", self.sz))?,
        );

        let side = parse_side(&self.side)?;

        // 注意 status 内嵌的 PartiallyFilled 同样是数量，必须用折算后的值
        let status = map_okx_order_state(&self.state);

        Ok(OrderUpdate {
            order_id: self.ord_id.clone(),
            client_order_id: self.cl_ord_id.clone(),
            exchange: Exchange::OKX,
            symbol: meta.symbol.clone(),
            side,
            status,
            quantity: sz,
        })
    }

    /// 转换为 Fill 事件（仅当 fill_sz > 0 时有效）。数量折算为币本位。
    pub fn to_fill(&self, meta: &SymbolMeta) -> Result<Option<Fill>, String> {
        let fill_sz = meta.qty_to_coin(
            f64::from_str(&self.fill_sz)
                .map_err(|_| format!("Failed to parse fill_sz: {}", self.fill_sz))?,
        );

        // 没有成交则不生成 Fill
        if fill_sz == 0.0 {
            return Ok(None);
        }

        let fill_px = f64::from_str(&self.fill_px)
            .map_err(|_| format!("Failed to parse fill_px: {}", self.fill_px))?;

        let side = parse_side(&self.side)?;

        // 必须用 fillFee（本次成交的手续费），不能用 fee —— 后者是该订单**累计**的
        // 手续费/返佣。用累计值当单笔会让分批成交的订单手续费被反复累加：一张分 3 次
        // 成交、每次 0.1 的单被记成 0.1+0.2+0.3=0.6，实际只有 0.3（N 笔均等成交高估
        // (N+1)/2 倍）。它进 TradingStats 的净利，进而影响 supervisor 的晋升/降级决策。
        // 顺带这也让两所口径一致：Binance 的 `n` 本就是单笔手续费。
        //
        // 走到这里说明确实有成交（上面已对 fill_sz == 0 提前返回），此时 fillFee 必然
        // 存在；缺失是交易所违约，如实报错而不是回退到 fee（那等于换个方式记错数）。
        let fill_fee = self.fill_fee.as_deref().ok_or_else(|| {
            format!("OKX 成交回报缺 fillFee 字段（订单 {}）", self.ord_id)
        })?;
        // OKX: 手续费为负数表示收费，取反统一为正数=收费
        let fee = f64::from_str(fill_fee)
            .map_err(|_| format!("Failed to parse fillFee: {fill_fee}"))?;

        let reason = match self.category.as_deref() {
            Some("adl") => crate::domain::FillReason::Adl,
            Some(c) if c.contains("liquidation") => crate::domain::FillReason::Liquidation,
            _ => crate::domain::FillReason::Normal,
        };

        Ok(Some(Fill {
            exchange: Exchange::OKX,
            symbol: meta.symbol.clone(),
            side,
            price: fill_px,
            size: fill_sz,
            client_order_id: self.cl_ord_id.clone(),
            order_id: self.ord_id.clone(),
            timestamp: now_ms(),
            fee: -fee,
            reason,
        }))
    }
}

/// OKX 订单状态映射
fn map_okx_order_state(state: &str) -> OrderStatus {
    match state {
        "live" => OrderStatus::Pending,
        "partially_filled" => OrderStatus::PartiallyFilled,
        "filled" => OrderStatus::Filled,
        "canceled" | "cancelled" => OrderStatus::Cancelled,
        other => OrderStatus::Rejected {
            reason: format!("Unknown state: {}", other),
        },
    }
}

/// Account Greeks 数据 (account-greeks channel)
///
/// OKX 的 `*BS` 后缀是 Black-Scholes 口径（另有 `*PA` 币本位口径，本项目不用）。
#[derive(Debug, Deserialize)]
pub struct GreeksData {
    pub ccy: String,
    #[serde(rename = "deltaBS")]
    pub delta_bs: String,
    #[serde(rename = "gammaBS")]
    pub gamma_bs: String,
    #[serde(rename = "thetaBS")]
    pub theta_bs: String,
    #[serde(rename = "vegaBS")]
    pub vega_bs: String,
    pub ts: String,
}

impl GreeksData {
    pub fn to_greeks(&self) -> Result<Greeks, String> {
        let delta = f64::from_str(&self.delta_bs)
            .map_err(|_| format!("Failed to parse deltaBS: {}", self.delta_bs))?;
        let gamma = f64::from_str(&self.gamma_bs)
            .map_err(|_| format!("Failed to parse gammaBS: {}", self.gamma_bs))?;
        let theta = f64::from_str(&self.theta_bs)
            .map_err(|_| format!("Failed to parse thetaBS: {}", self.theta_bs))?;
        let vega = f64::from_str(&self.vega_bs)
            .map_err(|_| format!("Failed to parse vegaBS: {}", self.vega_bs))?;
        let timestamp = self.ts.parse::<u64>()
            .map_err(|_| format!("Failed to parse timestamp: {}", self.ts))?;

        Ok(Greeks {
            exchange: Exchange::OKX,
            ccy: self.ccy.clone(),
            delta,
            gamma,
            theta,
            vega,
            timestamp,
        })
    }
}

/// WebSocket 事件响应
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct WsEvent {
    pub event: String,
    pub code: Option<String>,
    pub msg: Option<String>,
}

// ============================================================================
// Candle (K线)
// ============================================================================

/// OKX K线数据（字符串数组格式）
/// [ts, o, h, l, c, vol, volCcy, volCcyQuote, confirm]
pub type CandleRawData = Vec<String>;

/// 将 OKX K线原始数据转换为 Candle
pub fn parse_candle_data(
    raw: &CandleRawData,
    inst_id: &str,
    interval: CandleInterval,
) -> Result<Candle, String> {
    if raw.len() < 9 {
        return Err(format!("OKX candle data incomplete: {:?}", raw));
    }

    let symbol = from_okx(inst_id)
        .ok_or_else(|| format!("Unknown OKX symbol: {}", inst_id))?;
    let open_time: u64 = raw[0].parse()
        .map_err(|_| format!("Failed to parse candle ts: {}", raw[0]))?;
    let open = f64::from_str(&raw[1])
        .map_err(|_| format!("Failed to parse candle open: {}", raw[1]))?;
    let high = f64::from_str(&raw[2])
        .map_err(|_| format!("Failed to parse candle high: {}", raw[2]))?;
    let low = f64::from_str(&raw[3])
        .map_err(|_| format!("Failed to parse candle low: {}", raw[3]))?;
    let close = f64::from_str(&raw[4])
        .map_err(|_| format!("Failed to parse candle close: {}", raw[4]))?;
    let volume = f64::from_str(&raw[5])
        .map_err(|_| format!("Failed to parse candle vol: {}", raw[5]))?;
    let confirm = &raw[8] == "1";

    Ok(Candle {
        exchange: Exchange::OKX,
        symbol,
        interval,
        open_time,
        open,
        high,
        low,
        close,
        volume,
        confirm,
    })
}

/// CandleInterval → OKX bar 参数 (REST 和 WS channel 后缀)
///
/// OKX 格式恰好与 CandleInterval::Display 一致
pub fn candle_interval_to_okx_bar(interval: CandleInterval) -> String {
    interval.to_string()
}

/// OKX WS channel → CandleInterval
pub fn okx_channel_to_candle_interval(channel: &str) -> Option<CandleInterval> {
    match channel {
        "candle1m" => Some(CandleInterval::Min1),
        "candle3m" => Some(CandleInterval::Min3),
        "candle5m" => Some(CandleInterval::Min5),
        "candle15m" => Some(CandleInterval::Min15),
        "candle30m" => Some(CandleInterval::Min30),
        "candle1H" => Some(CandleInterval::Hour1),
        "candle2H" => Some(CandleInterval::Hour2),
        "candle4H" => Some(CandleInterval::Hour4),
        "candle6H" => Some(CandleInterval::Hour6),
        "candle12H" => Some(CandleInterval::Hour12),
        "candle1D" => Some(CandleInterval::Day1),
        "candle1W" => Some(CandleInterval::Week1),
        "candle1M" => Some(CandleInterval::Month1),
        "candle3M" => Some(CandleInterval::Month3),
        _ => None,
    }
}

#[cfg(test)]
mod trade_tests {
    use super::*;
    use crate::exchange::utils::StepFormatter;
    use std::sync::Arc;

    /// BTC-USDT-SWAP: ctVal = 0.01 BTC/张
    const CONTRACT_SIZE: f64 = 0.01;

    fn btc_meta() -> SymbolMeta {
        SymbolMeta {
            exchange: Exchange::OKX,
            symbol: "BTC".to_string(),
            price_formatter: Arc::new(StepFormatter::new(0.1)),
            size_step: 0.01,
            min_order_size: 0.01,
            contract_size: CONTRACT_SIZE,
        }
    }

    /// trades 频道推送样例（含本结构未声明的 count/source 字段，应被忽略）
    const TRADES_PUSH: &str = r#"{
        "arg":{"channel":"trades","instId":"BTC-USDT-SWAP"},
        "data":[{"instId":"BTC-USDT-SWAP","tradeId":"130639474","px":"42219.9",
                 "sz":"12","side":"sell","ts":"1630048897897","count":"3","source":"0"}]
    }"#;

    #[test]
    fn trade_parses_contracts_and_taker_side() {
        let push: WsPush<TradeData> = serde_json::from_str(TRADES_PUSH).expect("parse trades push");
        assert!(push.arg.inst_id.is_some(), "instId 必须存在");
        let t = push.data[0]
            .to_market_trade(&btc_meta())
            .expect("to_market_trade");
        assert_eq!(t.exchange, Exchange::OKX);
        assert_eq!(t.symbol, "BTC");
        assert_eq!(t.price, 42219.9);
        // 12 张 x 0.01 = 0.12 BTC —— 折算由签名强制，漏折算会是 12（差 100 倍）
        assert!((t.qty - 0.12).abs() < 1e-12, "got {}", t.qty);
        assert!(t.is_buyer_maker, "side=sell 是主动卖 -> 买方为挂单方");
        assert_eq!(t.timestamp, 1630048897897);
    }

    #[test]
    fn trade_buy_side_means_buyer_is_taker() {
        let raw = r#"{"tradeId":"1","px":"1.0","sz":"1","side":"buy","ts":"1"}"#;
        let d: TradeData = serde_json::from_str(raw).unwrap();
        let t = d.to_market_trade(&btc_meta()).unwrap();
        assert!(!t.is_buyer_maker);
    }

    /// **单边盘口是合法状态**：bids/asks 为空返回 Ok(None) 跳过，绝不能按坏报文
    /// 返回 Err —— 那会经 fail-fast 链路 kill actor、级联整机停机
    #[test]
    fn one_sided_book_is_skipped_not_fatal() {
        let raw = r#"{"asks":[],"bids":[["42219.9","12"]],"ts":"1630048897897"}"#;
        let d: BboData = serde_json::from_str(raw).unwrap();
        assert!(d.to_bbo(&btc_meta()).expect("不是错误").is_none());
        let raw = r#"{"asks":[["42220.0","30"]],"bids":[],"ts":"1630048897897"}"#;
        let d: BboData = serde_json::from_str(raw).unwrap();
        assert!(d.to_bbo(&btc_meta()).expect("不是错误").is_none());
    }

    /// Critical 回归防线：OKX 盘口挂单量原先未折算，导致 BBO 的 qty 是张数、
    /// 而 position/fill/trade 是币本位，spread_arb 按币本位使用 ask_qty/bid_qty
    /// 时 OKX 腿的流动性约束实际失效（差 contract_size 倍）。
    #[test]
    fn bbo_quantities_are_converted_to_coin() {
        let raw = r#"{"asks":[["42220.0","30"]],"bids":[["42219.9","12"]],"ts":"1630048897897"}"#;
        let d: BboData = serde_json::from_str(raw).unwrap();
        let bbo = d.to_bbo(&btc_meta()).unwrap().expect("双边盘口");
        assert_eq!(bbo.symbol, "BTC");
        assert_eq!(bbo.bid_price, 42219.9);
        assert!((bbo.bid_qty - 0.12).abs() < 1e-12, "got {}", bbo.bid_qty);
        assert!((bbo.ask_qty - 0.30).abs() < 1e-12, "got {}", bbo.ask_qty);
    }

    /// 持仓同样折算，且方向符号保留
    #[test]
    fn position_size_is_converted_to_coin_keeping_sign() {
        let raw = r#"{"instId":"BTC-USDT-SWAP","instType":"SWAP","pos":"-12","posSide":"net",
                      "avgPx":"42000","upl":"1.5","lever":"3","mgnMode":"cross"}"#;
        let d: PositionData = serde_json::from_str(raw).unwrap();
        let p = d.to_position("BTC".to_string(), CONTRACT_SIZE).unwrap();
        assert_eq!(p.symbol, "BTC");
        assert!((p.size - (-0.12)).abs() < 1e-12, "got {}", p.size);
    }

    /// 空仓时 OKX 的数值字段是**空串**，按 0 处理而非解析失败
    #[test]
    fn flat_position_with_empty_numeric_fields_is_zero() {
        let raw = r#"{"instId":"BTC-USDT-SWAP","pos":"","posSide":"net","avgPx":"","upl":""}"#;
        let d: PositionData = serde_json::from_str(raw).unwrap();
        let p = d.to_position("BTC".to_string(), CONTRACT_SIZE).unwrap();
        assert_eq!(p.size, 0.0);
    }


    /// **Critical 防线**：双向持仓模式必须报错，不能按净持仓口径解析。
    /// 该模式下 `pos` 恒为正数、方向靠 posSide 表达，静默解析会让空头被当成多头，
    /// 策略随后朝反方向加仓。
    #[test]
    fn long_short_mode_is_rejected_not_silently_misread() {
        for pos_side in ["long", "short"] {
            let raw = format!(
                r#"{{"instId":"BTC-USDT-SWAP","pos":"12","posSide":"{pos_side}",
                     "avgPx":"42000","upl":"0"}}"#
            );
            let d: PositionData = serde_json::from_str(&raw).unwrap();
            let err = d
                .to_position("BTC".to_string(), CONTRACT_SIZE)
                .expect_err("双向持仓模式必须报错");
            assert!(err.contains("单向净持仓"), "错误信息应指出如何修复: {err}");
        }
    }

    #[test]
    fn trade_unknown_side_is_error_not_silent() {
        let raw = r#"{"tradeId":"1","px":"1.0","sz":"1","side":"","ts":"1"}"#;
        let d: TradeData = serde_json::from_str(raw).unwrap();
        assert!(d.to_market_trade(&btc_meta()).is_err());
    }

    /// **Critical 回归防线**：单笔成交的手续费必须取 `fillFee`，不能取 `fee`。
    ///
    /// OKX 的 `fee` 是该订单**累计**手续费/返佣，`fillFee` 才是"last filled fee"。
    /// 用累计值当单笔，分批成交的订单手续费会被反复累加（3 次均等成交高估 2 倍），
    /// 错的数字进 TradingStats 净利，再进 supervisor 的晋升/降级决策。
    #[test]
    fn fill_fee_is_used_instead_of_cumulative_fee() {
        // 第三次推送：本次成交手续费 -0.1，而订单累计已达 -0.3
        let raw = r#"{"instId":"BTC-USDT-SWAP","ordId":"1","clOrdId":"x-a","side":"buy",
            "state":"partially_filled","px":"42000","sz":"3","fillSz":"1","fillPx":"42000",
            "accFillSz":"3","avgPx":"42000","fee":"-0.3","fillFee":"-0.1","feeCcy":"USDT"}"#;
        let d: OrderPushData = serde_json::from_str(raw).unwrap();
        let meta = btc_meta();
        let fill = d.to_fill(&meta).unwrap().expect("有成交必产出 Fill");
        assert!(
            (fill.fee - 0.1).abs() < 1e-12,
            "取成了订单累计手续费（{}），分批成交会把手续费反复累加",
            fill.fee
        );
    }

    /// 有成交却没有 fillFee 是交易所违约：如实报错，不回退到累计的 fee（那是换个方式记错数）
    #[test]
    fn missing_fill_fee_on_a_real_fill_is_an_error() {
        let raw = r#"{"instId":"BTC-USDT-SWAP","ordId":"1","clOrdId":"x-a","side":"buy",
            "state":"partially_filled","px":"42000","sz":"3","fillSz":"1","fillPx":"42000",
            "accFillSz":"3","avgPx":"42000","fee":"-0.3","feeCcy":"USDT"}"#;
        let d: OrderPushData = serde_json::from_str(raw).unwrap();
        assert!(d.to_fill(&btc_meta()).is_err(), "缺 fillFee 应报错而非回退到 fee");
    }
}
