use super::from_binance;
use crate::domain::{
    Balance, Exchange, Fill, FundingRate, IndexPrice, MarkPrice, MarketTrade, OrderStatus,
    OrderUpdate, Side, now_ms, BBO,
};
use serde::Deserialize;
use std::str::FromStr;

/// Mark Price 更新 (包含资金费率)
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct MarkPriceUpdate {
    pub e: String,
    pub s: String,
    pub p: String,
    pub i: String,
    pub r: String,
    #[serde(rename = "T")]
    pub t: i64,
}

impl MarkPriceUpdate {
    /// 转换为 FundingRate
    /// quote: 计价币种（用于解析 symbol）
    /// timestamp: 数据时间戳（毫秒）
    pub fn to_funding_rate(&self, quote: &str, timestamp: u64) -> Result<FundingRate, String> {
        let symbol = from_binance(&self.s, quote)
            .ok_or_else(|| format!("Unknown Binance symbol: {}", self.s))?;
        let rate = f64::from_str(&self.r)
            .map_err(|_| format!("Failed to parse funding rate: {}", self.r))?;

        Ok(FundingRate {
            exchange: Exchange::Binance,
            symbol,
            rate,
            next_settle_time: self.t as u64,
            timestamp,
        })
    }

    /// 转换为 MarkPrice
    pub fn to_mark_price(&self, quote: &str, timestamp: u64) -> Result<MarkPrice, String> {
        let symbol = from_binance(&self.s, quote)
            .ok_or_else(|| format!("Unknown Binance symbol: {}", self.s))?;
        let price = f64::from_str(&self.p)
            .map_err(|_| format!("Failed to parse mark price: {}", self.p))?;

        Ok(MarkPrice {
            exchange: Exchange::Binance,
            symbol,
            price,
            timestamp,
        })
    }

    /// 转换为 IndexPrice
    pub fn to_index_price(&self, quote: &str, timestamp: u64) -> Result<IndexPrice, String> {
        let symbol = from_binance(&self.s, quote)
            .ok_or_else(|| format!("Unknown Binance symbol: {}", self.s))?;
        let price = f64::from_str(&self.i)
            .map_err(|_| format!("Failed to parse index price: {}", self.i))?;

        Ok(IndexPrice {
            exchange: Exchange::Binance,
            symbol,
            price,
            timestamp,
        })
    }

    /// 获取 symbol (用于查询结算间隔)
    pub fn symbol(&self, quote: &str) -> Result<crate::domain::Symbol, String> {
        from_binance(&self.s, quote)
            .ok_or_else(|| format!("Unknown Binance symbol: {}", self.s))
    }
}

/// Book Ticker (BBO)
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct BookTicker {
    pub e: String,
    pub s: String,
    pub b: String,
    #[serde(rename = "B")]
    pub bid_qty: String,
    pub a: String,
    #[serde(rename = "A")]
    pub ask_qty: String,
    #[serde(rename = "T")]
    pub t: i64,
}

impl BookTicker {
    pub fn to_bbo(&self, quote: &str) -> Result<BBO, String> {
        let symbol = from_binance(&self.s, quote)
            .ok_or_else(|| format!("Unknown Binance symbol: {}", self.s))?;
        let bid_price = f64::from_str(&self.b)
            .map_err(|_| format!("Failed to parse bid price: {}", self.b))?;
        let bid_qty = f64::from_str(&self.bid_qty)
            .map_err(|_| format!("Failed to parse bid qty: {}", self.bid_qty))?;
        let ask_price = f64::from_str(&self.a)
            .map_err(|_| format!("Failed to parse ask price: {}", self.a))?;
        let ask_qty = f64::from_str(&self.ask_qty)
            .map_err(|_| format!("Failed to parse ask qty: {}", self.ask_qty))?;

        Ok(BBO {
            exchange: Exchange::Binance,
            symbol,
            bid_price,
            bid_qty,
            ask_price,
            ask_qty,
            timestamp: self.t as u64,
        })
    }
}

/// 归集成交 (`<symbol>@aggTrade` stream)
///
/// 官方字段：`e` 事件类型、`E` 事件时间、`s` 交易对、`a` 归集成交 ID、`p` 价格、
/// `q` 数量、`f`/`l` 首末成交 ID、`T` 成交时间、`m` 买方是否为挂单方。
/// 本结构只声明所需字段，其余 (含后续新增的 `nq`/`st`) 由 serde 忽略。
///
/// 时间戳取 `T` (成交时间) 而非 `E` (事件推送时间)，与 `BookTicker` 取 `T` 的口径一致。
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct AggTrade {
    pub e: String,
    pub s: String,
    pub p: String,
    pub q: String,
    #[serde(rename = "T")]
    pub t: i64,
    pub m: bool,
}

impl AggTrade {
    pub fn to_market_trade(&self, quote: &str) -> Result<MarketTrade, String> {
        let symbol = from_binance(&self.s, quote)
            .ok_or_else(|| format!("Unknown Binance symbol: {}", self.s))?;
        let price = f64::from_str(&self.p)
            .map_err(|_| format!("Failed to parse trade price: {}", self.p))?;
        let qty = f64::from_str(&self.q)
            .map_err(|_| format!("Failed to parse trade qty: {}", self.q))?;

        Ok(MarketTrade {
            exchange: Exchange::Binance,
            symbol,
            price,
            qty,
            is_buyer_maker: self.m,
            timestamp: self.t as u64,
        })
    }
}

/// 账户更新
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct AccountUpdate {
    pub e: String,
    pub a: AccountData,
}

/// `ACCOUNT_UPDATE.a`
///
/// **有意不声明 `P`（持仓）**：那是持仓快照，而持仓的维护模型是「启动期 REST 基线 + 之后
/// 全程 Fill 累加」（见 [`crate::messaging::ExchangeEventData::PositionBaseline`]）。校验本地
/// 持仓是否漂移走 `PositionReport` 通道，不从这条推送取。
#[derive(Debug, Deserialize)]
pub struct AccountData {
    #[serde(rename = "B")]
    pub balances: Vec<AccountBalance>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct AccountBalance {
    pub a: String,
    pub wb: String,
    pub cw: String,
    pub bc: String,
}

impl AccountBalance {
    pub fn to_balance(&self) -> Result<Balance, String> {
        let available = f64::from_str(&self.cw)
            .map_err(|_| format!("Failed to parse available balance: {}", self.cw))?;
        let wallet_balance = f64::from_str(&self.wb)
            .map_err(|_| format!("Failed to parse wallet balance: {}", self.wb))?;
        let frozen = (wallet_balance - available).max(0.0);

        Ok(Balance {
            exchange: Exchange::Binance,
            asset: self.a.clone(),
            available,
            frozen,
        })
    }
}

/// 订单更新
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct OrderTradeUpdate {
    pub e: String,
    pub o: OrderData,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct OrderData {
    pub s: String,
    pub c: String,  // client_order_id
    #[serde(rename = "S")]
    pub side: String, // "BUY" or "SELL"
    pub i: i64,
    #[serde(rename = "X")]
    pub status: String,
    pub q: String,
    /// 委托价 (Original price)。挂单态 ap 是 "0"，OrderUpdate.price 必须用它 ——
    /// 否则本地按 update 重建挂单时会造出 price=0 的假单
    pub p: String,
    /// 累计成交量 (Cumulative filled quantity)
    pub z: String,
    /// 本次成交量 (Last filled quantity)
    pub l: String,
    /// 本次成交价格 (Last filled price)
    #[serde(rename = "L")]
    pub last_price: String,
    pub ap: String,
    pub rp: String,
    /// 本次成交手续费 (Commission amount)。
    ///
    /// **可能缺失**：Binance 文档对 `n`/`N` 明确写着 "will not push if no commission"
    /// —— NEW / CANCELED 这类无成交事件不带该字段。此前声明为必填 `String`，于是一条
    /// **完全合法**的回报会让整个 `OrderTradeUpdate` 反序列化失败 → `WsError::ParseError`
    /// → kill actor → 级联整机停机。schema 必须如实描述交易所会发什么。
    ///
    /// 有成交时它必然存在，所以 [`OrderTradeUpdate::to_fill`] 里缺失即报错（那才是真违约）。
    #[serde(default)]
    pub n: Option<String>,
    /// reduce-only 标志
    #[serde(rename = "R", default)]
    pub reduce_only: bool,
}

impl OrderTradeUpdate {
    pub fn to_order_update(&self, quote: &str) -> Result<OrderUpdate, String> {
        let symbol = from_binance(&self.o.s, quote)
            .ok_or_else(|| format!("Unknown Binance symbol: {}", self.o.s))?;
        let filled_qty = f64::from_str(&self.o.z)
            .map_err(|_| format!("Failed to parse filled qty: {}", self.o.z))?;
        let fill_sz = f64::from_str(&self.o.l)
            .map_err(|_| format!("Failed to parse last filled qty: {}", self.o.l))?;

        let side = match self.o.side.as_str() {
            "BUY" => Side::Long,
            "SELL" => Side::Short,
            other => return Err(format!("Unknown Binance side: {}", other)),
        };

        let status = match self.o.status.as_str() {
            "NEW" => OrderStatus::Pending,
            "PARTIALLY_FILLED" => OrderStatus::PartiallyFilled { filled: filled_qty },
            "FILLED" => OrderStatus::Filled,
            "CANCELED" | "CANCELLED" => OrderStatus::Cancelled,
            "REJECTED" | "EXPIRED" => OrderStatus::Rejected {
                reason: self.o.status.clone(),
            },
            other => OrderStatus::Rejected {
                reason: format!("Unknown status: {}", other),
            },
        };

        // OrderUpdate.price 的语义是**委托价**（OKX 用 px、HL 用 limit_px，口径一致）。
        // 此前误用均价 ap：挂单态（NEW）的 ap 是 "0"，流进 SymbolState 的挂单重建路径
        // 会造出 price=0 的假单。市价单没有委托价，Binance 推 "0"，如实透传。
        let price = f64::from_str(&self.o.p)
            .map_err(|_| format!("Failed to parse order price: {}", self.o.p))?;
        let quantity = f64::from_str(&self.o.q)
            .map_err(|_| format!("Failed to parse quantity: {}", self.o.q))?;

        Ok(OrderUpdate {
            order_id: self.o.i.to_string(),
            client_order_id: if self.o.c.is_empty() { None } else { Some(self.o.c.clone()) },
            exchange: Exchange::Binance,
            symbol,
            side,
            status,
            price,
            reduce_only: self.o.reduce_only,
            quantity,
            filled_quantity: filled_qty,
            fill_sz,
            timestamp: now_ms(),
        })
    }

    /// 转换为 Fill 事件（仅当 fill_sz > 0 时有效）
    pub fn to_fill(&self, quote: &str) -> Result<Option<Fill>, String> {
        let fill_sz = f64::from_str(&self.o.l)
            .map_err(|_| format!("Failed to parse last filled qty: {}", self.o.l))?;

        // 没有成交则不生成 Fill
        if fill_sz == 0.0 {
            return Ok(None);
        }

        let symbol = from_binance(&self.o.s, quote)
            .ok_or_else(|| format!("Unknown Binance symbol: {}", self.o.s))?;
        let last_price = f64::from_str(&self.o.last_price)
            .map_err(|_| format!("Failed to parse last price: {}", self.o.last_price))?;

        let side = match self.o.side.as_str() {
            "BUY" => Side::Long,
            "SELL" => Side::Short,
            other => return Err(format!("Unknown Binance side: {}", other)),
        };

        // Binance: n 为正数表示收费
        // 走到这里说明确实有成交（上面已对 fill_sz == 0 提前返回），此时 Binance 必推
        // commission —— 缺失是交易所违约，如实报错而不是兜 0（兜 0 会让净利长期高估）
        let fee_raw = self.o.n.as_deref().ok_or_else(|| {
            format!("Binance 成交回报缺 commission 字段（订单 {}）", self.o.i)
        })?;
        let fee = f64::from_str(fee_raw)
            .map_err(|_| format!("Failed to parse commission: {fee_raw}"))?;

        Ok(Some(Fill {
            exchange: Exchange::Binance,
            symbol,
            side,
            price: last_price,
            size: fill_sz,
            client_order_id: if self.o.c.is_empty() { None } else { Some(self.o.c.clone()) },
            order_id: self.o.i.to_string(),
            timestamp: now_ms(),
            fee,
            // Binance 强平/ADL 也经 ORDER_TRADE_UPDATE 以 fill 到达并如常更新持仓；
            // 本 codec 暂未解析订单类型区分来源，统一标 Normal。
            reason: crate::domain::FillReason::Normal,
        }))
    }
}

/// WebSocket 订阅响应
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct WsResponse {
    pub result: Option<serde_json::Value>,
    pub error: Option<WsError>,
}

/// WebSocket 错误
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct WsError {
    pub code: i32,
    pub msg: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 官方 aggTrade 样例载荷（含本结构未声明的 a/f/l 字段，应被忽略）
    const AGG_TRADE: &str = r#"{
        "e":"aggTrade","E":123456789,"s":"BTCUSDT","a":5933014,
        "p":"42219.90","q":"0.125","f":100,"l":105,"T":123456785,"m":true
    }"#;

    #[test]
    fn agg_trade_parses_price_qty_and_taker_side() {
        let agg: AggTrade = serde_json::from_str(AGG_TRADE).expect("parse aggTrade");
        let t = agg.to_market_trade("USDT").expect("to_market_trade");
        assert_eq!(t.exchange, Exchange::Binance);
        assert_eq!(t.symbol, "BTC");
        assert_eq!(t.price, 42219.90);
        assert_eq!(t.qty, 0.125);
        assert!(t.is_buyer_maker, "m=true -> 买方挂单 -> 主动卖");
        // 时间戳取 T (成交时间) 而非 E (推送时间)
        assert_eq!(t.timestamp, 123456785);
    }

    #[test]
    fn agg_trade_unknown_quote_is_error_not_silent() {
        let agg: AggTrade = serde_json::from_str(AGG_TRADE).unwrap();
        assert!(agg.to_market_trade("USDC").is_err());
    }
}
