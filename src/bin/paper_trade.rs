//! 模拟盘入口：真实实盘行情 + 本地柜台撮合，**不需要任何凭证**。
//!
//! 与实盘 (`spread_arb`) 共用同一份策略与行情链路，差异只在 outcome 事件流向 ——
//! 订单落 `PaperCounterActor` 的本地柜台，永不发往交易所。
//!
//! 运行:
//! ```sh
//! cargo run --bin paper_trade -- paper.json
//! ```
//!
//! 配置示例（`exchanges.*.credentials` 省略即只接公共行情）：
//! ```json
//! {
//!   "exchanges": { "binance": { "quote": "USDT" } },
//!   "paper": {
//!     "initial_balance_usdt": 10000.0,
//!     "maker_fee_rate": 0.0002,
//!     "taker_fee_rate": 0.0005,
//!     "order_to_exchange_delay_ms": 50
//!   },
//!   "symbols": ["BTC"],
//!   "offset_bp": 1.0,
//!   "order_notional": 200.0
//! }
//! ```

use anyhow::Context;
use hft_engine_rs::domain::{Exchange, Order, OrderType, Side, Symbol, TimeInForce};
use hft_engine_rs::engine::{
    init_tracing, load_config, wait_for_shutdown, AddStrategies, ManagerActor, ManagerActorArgs,
    setup_binance, setup_hyperliquid, setup_okx,
    StrategySpec,
};
use hft_engine_rs::exchange::binance::BinanceCredentials;
use hft_engine_rs::exchange::hyperliquid::HyperliquidCredentials;
use hft_engine_rs::exchange::okx::OkxCredentials;
use hft_engine_rs::exchange::{ExchangeAccess, SubscriptionKind};
use hft_engine_rs::messaging::{IncomeEvent, MarketData, MarketEvent};
use hft_engine_rs::sim::SimConfig;
use hft_engine_rs::strategy::{OutcomeEvent, Strategy, StrategyView};
use kameo::actor::Spawn;
use kameo::mailbox;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};

/// 交易所接入配置：模拟盘下 `credentials` 可整段省略
#[derive(Debug, Clone, Deserialize)]
struct ExchangesConfig {
    binance: Option<ExchangeAccess<BinanceCredentials>>,
    okx: Option<ExchangeAccess<OkxCredentials>>,
    hyperliquid: Option<ExchangeAccess<HyperliquidCredentials>>,
}

#[derive(Debug, Clone, Deserialize)]
struct PaperTradeConfig {
    exchanges: ExchangesConfig,
    paper: SimConfig,
    /// 观测标的（内部基础符号，如 "BTC"）
    symbols: Vec<Symbol>,
    /// 挂单相对买价的下偏幅度 (bp)
    offset_bp: f64,
    /// 单笔下单名义额 (USDT)
    order_notional: f64,
}

/// 最小挂单策略：空仓且无挂单时，在买价下方 `offset_bp` 挂一张 PostOnly 买单。
///
/// 只用于把模拟盘链路跑通并观察成交/净值；不是生产策略。
struct DipMaker {
    exchange: Exchange,
    symbols: Vec<Symbol>,
    offset_bp: f64,
    order_notional: f64,
}

impl Strategy for DipMaker {
    fn public_streams(&self) -> HashMap<Exchange, HashSet<SubscriptionKind>> {
        let kinds = self
            .symbols
            .iter()
            .flat_map(|s| {
                [
                    SubscriptionKind::BBO { symbol: s.clone() },
                    SubscriptionKind::Trades { symbol: s.clone() },
                ]
            })
            .collect();
        HashMap::from([(self.exchange, kinds)])
    }

    fn order_timeout_ms(&self) -> u64 {
        10_000
    }

    fn on_event(&mut self, event: &IncomeEvent, view: StrategyView<'_>) -> Vec<OutcomeEvent> {
        let IncomeEvent::Market(MarketEvent {
            data: MarketData::BBO(bbo),
            ..
        }) = event
        else {
            return Vec::new();
        };
        let Some(symbol_state) = view.symbol(&bbo.symbol) else {
            return Vec::new();
        };
        // 有仓位或有挂单就不动（本演示策略不做平仓）
        if symbol_state.position_size(self.exchange).abs() > 0.0
            || symbol_state.pending_orders().next().is_some()
        {
            return Vec::new();
        }

        let price = bbo.bid_price * (1.0 - self.offset_bp / 10_000.0);
        let quantity = self.order_notional / price;
        vec![OutcomeEvent::PlaceOrders {
            orders: vec![Order {
                id: String::new(),
                exchange: self.exchange,
                symbol: bbo.symbol.clone(),
                side: Side::Long,
                order_type: OrderType::Limit {
                    price,
                    tif: TimeInForce::PostOnly,
                },
                quantity,
                reduce_only: false,
                client_order_id: String::new(),
            }],
            comment: format!(
                "paper_dip_buy | bid={} | offset={}bp | notional={}",
                bbo.bid_price, self.offset_bp, self.order_notional
            ),
        }]
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing()?;
    let config: PaperTradeConfig = load_config("paper.json")?;

    // 只支持单所演示：取配置里第一个启用的所
    let exchange = if config.exchanges.binance.is_some() {
        Exchange::Binance
    } else if config.exchanges.okx.is_some() {
        Exchange::OKX
    } else if config.exchanges.hyperliquid.is_some() {
        Exchange::Hyperliquid
    } else {
        anyhow::bail!("exchanges 至少要启用一个交易所");
    };

    tracing::warn!(
        %exchange,
        symbols = ?config.symbols,
        initial_balance = config.paper.initial_balance_usdt,
        order_delay_ms = config.paper.order_to_exchange_delay_ms,
        "PAPER TRADING — 订单只落本地柜台，不会发往任何交易所"
    );

    // 装配参与的交易所（缺省即该所不参与）—— manager 对"有哪些所"无知
    let mut exchanges = Vec::new();
    if let Some(access) = config.exchanges.binance.clone() {
        exchanges.push(setup_binance(access)?);
    }
    if let Some(access) = config.exchanges.okx.clone() {
        exchanges.push(setup_okx(access)?);
    }
    if let Some(access) = config.exchanges.hyperliquid.clone() {
        exchanges.push(setup_hyperliquid(access)?);
    }

    let manager = ManagerActor::spawn_with_mailbox(
        ManagerActorArgs {
            exchanges,
            paper: config.paper,
        },
        mailbox::unbounded(),
    );
    manager
        .wait_for_startup_result()
        .await
        .context("ManagerActor 启动失败")?;

    let strategy = DipMaker {
        exchange,
        symbols: config.symbols.clone(),
        offset_bp: config.offset_bp,
        order_notional: config.order_notional,
    };
    manager
        // 绑定到按 symbol 命名的模拟账户；实盘账户不注册任何策略，故不会有真实下单
        .ask(AddStrategies(vec![StrategySpec::paper(
            Box::new(strategy),
            config.symbols.join(","),
        )]))
        .send()
        .await
        .context("添加策略失败")?;

    // 停机信号 → Ok；manager 意外终止 → Err，进程带着原因非零退出
    wait_for_shutdown(manager).await
}
