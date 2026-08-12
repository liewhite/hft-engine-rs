//! 回测入口：用币安官方历史 trades 还原行情，对策略做确定性回测。
//!
//! 与实盘**完全相同的策略代码** —— 差异只在驱动层 (单线程虚拟时间 vs 实盘并发墙钟)。
//! 成交流用 `aggTrades` (归集口径)，与实盘 WS 的 aggTrade 对齐，避免因子在回测/实盘间漂移。
//! 首跑联网下载历史数据并落 `data-cache`，二跑命中缓存。
//!
//! 运行: `cargo run --bin backtest -- [SYMBOL] [START yyyy-mm-dd] [END yyyy-mm-dd]`
//! 例:   `cargo run --bin backtest -- BTC 2024-01-01 2024-01-01`
//!
//! SYMBOL 是**内部基础符号**（`BTC`），计价币由代码补齐；传 `BTCUSDT` 会被拼成
//! `BTCUSDTUSDT`，数据源稳定 404 且只 warn，表现为"回测跑完但 0 事件"。

use anyhow::Context;
use chrono::NaiveDate;
use hft_engine_rs::backtest::{BacktestEngine, BinanceDataKind, BinanceHistory};
use hft_engine_rs::domain::{
    Exchange, Order, OrderType, Side, Symbol, SymbolMeta, TimeInForce,
};
use hft_engine_rs::engine::{SequentialClientOrderIdGen, StrategyRunner};
use hft_engine_rs::exchange::binance::BinanceClient;
use hft_engine_rs::exchange::{ExchangeClient, SubscriptionKind};
use hft_engine_rs::messaging::{AccountData, IncomeEvent, MarketData};
use hft_engine_rs::sim::SimConfig;
use hft_engine_rs::strategy::{OutcomeEvent, Strategy, StrategyView};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

const EX: Exchange = Exchange::Binance;

/// 最小均值回归 maker 演示策略：仅用于跑通 trade-native 全链路 (非生产策略)。
///
/// 空仓时在最新成交价下方挂 PostOnly 买单 (价格下穿即 maker 成交)；持多后在自己记的开仓价
/// 上方挂 reduce-only PostOnly 卖单止盈。有挂单时静默等待，避免重复下单。
///
/// **开仓价由策略自己从成交流记录**：框架的持仓只有数量（见 `crate::domain::Position`）。
/// 这不是缺失而是分工 —— "均价"有多种口径（加权平均 / 最后一笔 / FIFO），只有策略自己知道
/// 它要哪种；框架替所有人猜一种，反而是错的那种的人还得绕开它。
struct MeanRevertMaker {
    symbol: Symbol,
    offset_ratio: f64,
    order_size: f64,
    /// 本策略的持仓成本：按成交加权，空仓时清零
    entry_price: f64,
    entry_size: f64,
}

impl MeanRevertMaker {
    fn place(&self, side: Side, price: f64, qty: f64, reduce_only: bool, comment: &str) -> OutcomeEvent {
        OutcomeEvent::PlaceOrders {
            orders: vec![Order {
                id: String::new(),
                exchange: EX,
                symbol: self.symbol.clone(),
                side,
                order_type: OrderType::Limit {
                    price,
                    tif: TimeInForce::PostOnly,
                },
                quantity: qty,
                reduce_only,
                client_order_id: String::new(),
            }],
            comment: comment.to_string(),
        }
    }
}

impl Strategy for MeanRevertMaker {
    fn public_streams(&self) -> HashMap<Exchange, HashSet<SubscriptionKind>> {
        let mut kinds = HashSet::new();
        // 仅用于把 (EX, symbol) 注册进订阅范围；trade 事件按 (exchange,symbol) 路由即可送达
        kinds.insert(SubscriptionKind::BBO { symbol: self.symbol.clone() });
        let mut m = HashMap::new();
        m.insert(EX, kinds);
        m
    }

    fn order_timeout_ms(&self) -> u64 {
        0
    }

    fn on_event(&mut self, event: &IncomeEvent, view: StrategyView<'_>) -> Vec<OutcomeEvent> {
        // 自己跟成交维护开仓成本（加权平均；平完即清零）
        if let IncomeEvent::Account(a) = event {
            let AccountData::Fill(f) = &a.data else {
                return Vec::new();
            };
            match f.side {
                Side::Long => {
                    let total = self.entry_size + f.size;
                    self.entry_price =
                        (self.entry_size * self.entry_price + f.size * f.price) / total;
                    self.entry_size = total;
                }
                Side::Short => {
                    self.entry_size = (self.entry_size - f.size).max(0.0);
                    if self.entry_size < 1e-12 {
                        self.entry_price = 0.0;
                    }
                }
            }
            return Vec::new();
        }
        let IncomeEvent::Market(m) = event else {
            return Vec::new();
        };
        let MarketData::MarketTrade(t) = &m.data else {
            return Vec::new();
        };
        let Some(symbol_state) = view.symbol(&self.symbol) else {
            return Vec::new();
        };
        if symbol_state.has_pending_orders() {
            return Vec::new();
        }
        let pos_size = symbol_state.position_size(EX);

        if pos_size.abs() < 1e-9 {
            // 空仓：在成交价下方挂买单
            let price = t.price * (1.0 - self.offset_ratio);
            vec![self.place(Side::Long, price, self.order_size, false, "open_long")]
        } else if pos_size > 0.0 {
            // 持多：在自己记的开仓价上方挂 reduce-only 卖单止盈
            let entry = if self.entry_price > 0.0 {
                self.entry_price
            } else {
                t.price
            };
            let price = entry * (1.0 + self.offset_ratio);
            vec![self.place(Side::Short, price, pos_size.abs(), true, "take_profit")]
        } else {
            Vec::new()
        }
    }
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    // 内部基础符号 (如 "BTC")，计价币单独给数据源还原币安文件名
    let symbol: Symbol = args.first().cloned().unwrap_or_else(|| "BTC".to_string());
    let quote = "USDT";
    let start_str = args.get(1).cloned().unwrap_or_else(|| "2024-01-01".to_string());
    let end_str = args.get(2).cloned().unwrap_or_else(|| start_str.clone());
    let start = NaiveDate::parse_from_str(&start_str, "%Y-%m-%d").context("parse START date")?;
    let end = NaiveDate::parse_from_str(&end_str, "%Y-%m-%d").context("parse END date")?;

    // 真实 Binance (无凭证) 拉取 symbol 元数据 (精度)，与实盘一致 —— 仅此一处需要 async
    let client = BinanceClient::new("USDT".to_string(), None).map_err(|e| anyhow::anyhow!("binance client: {e}"))?;
    let metas_vec = tokio::runtime::Runtime::new()?
        .block_on(client.fetch_all_symbol_metas())
        .map_err(|e| anyhow::anyhow!("fetch symbol metas: {e}"))?;
    let symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>> = Arc::new(
        metas_vec
            .into_iter()
            .map(|m| ((m.exchange, m.symbol.clone()), m))
            .collect(),
    );

    let source = BinanceHistory::source(
        std::slice::from_ref(&symbol),
        quote,
        start,
        end,
        false, // trade-native: 直接用真实 trade 撮合
        "data-cache",
        &[BinanceDataKind::AggTrades],
    )
    .map_err(|e| anyhow::anyhow!("build history source: {e}"))?;

    let strategy = MeanRevertMaker {
        symbol: symbol.clone(),
        offset_ratio: 0.0003,
        order_size: 0.01,
        entry_price: 0.0,
        entry_size: 0.0,
    };
    // 回测用确定性 id 生成器 -> 同一输入同一逐笔回报序列
    let runner = StrategyRunner::with_id_gen(
        Box::new(strategy),
        symbol_metas.clone(),
        Box::new(SequentialClientOrderIdGen::default()),
    );
    let config = SimConfig {
        exchange_to_strategy_delay_ms: 100,
        order_to_exchange_delay_ms: 50,
        initial_balance_usdt: 10_000.0,
        ..SimConfig::default()
    };

    let mut engine = BacktestEngine::new(source.as_ref(), vec![runner], config, symbol_metas);
    let result = engine.run();

    println!("==================== Backtest Result ====================");
    println!("symbol         : {symbol}  [{start} .. {end}]");
    println!("market events  : {}", result.market_events);
    println!("fills          : {}", result.fills);
    println!("realized PnL   : {:.6}", result.realized_pnl);
    println!(
        "final equity   : {:.6}  (init {:.2})",
        result.final_equity, result.initial_balance
    );
    println!("open positions : {:?}", result.positions);
    println!("=========================================================");
    Ok(())
}
