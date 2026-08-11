//! 自适应晋升入口：按成交额取 Top N 个 symbol，每个常驻一个模拟账户；判据认为可以时拉起
//! 该 symbol 的实盘实例，判据认为该收时撤下并立即平仓。
//!
//! **唯一需要你写的是 [`MyPolicy::decide`]** —— 框架其余部分已经接好。未实现前它返回
//! `Hold`，即只跑模拟、只积累样本、永不真实下单（这也是安全的默认行为）。
//!
//! 运行:
//! ```sh
//! cargo run --bin adaptive_trade -- adaptive.json
//! ```
//!
//! 配置（模拟不需要凭证；**只有真的要晋升到实盘时才需要填 credentials**）：
//! ```json
//! {
//!   "exchanges": { "binance": { "quote": "USDT" } },
//!   "paper": {
//!     "initial_balance_usdt": 10000.0,
//!     "maker_fee_rate": 0.0002,
//!     "taker_fee_rate": 0.0005,
//!     "order_to_exchange_delay_ms": 50
//!   },
//!   "top_n": 10,
//!   "offset_bp": 0.0,
//!   "order_notional": 200.0
//! }
//! ```

use anyhow::Context;
use hft_engine_rs::domain::{Exchange, Order, OrderType, Side, Symbol, TimeInForce};
use hft_engine_rs::engine::{
    init_tracing, load_config, spawn_supervised, wait_for_shutdown, Decision, ManagerActor,
    ManagerActorArgs,
    PromotionPolicy, SubscribeAccount, SubscribeMarket, SupervisorActor, SupervisorArgs, SymbolView,
};
use hft_engine_rs::exchange::binance::{BinanceClient, BinanceCredentials};
use hft_engine_rs::exchange::hyperliquid::HyperliquidCredentials;
use hft_engine_rs::exchange::okx::OkxCredentials;
use hft_engine_rs::exchange::{ExchangeAccess, ExchangeClient, SubscriptionKind};
use hft_engine_rs::messaging::{IncomeEvent, MarketData, MarketEvent, StateManager};
use hft_engine_rs::sim::SimConfig;
use hft_engine_rs::strategy::{OutcomeEvent, Strategy};
use kameo::actor::Spawn;
use kameo::mailbox;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};

const EXCHANGE: Exchange = Exchange::Binance;

// ============================================================================
// 你要写的部分
// ============================================================================

/// 晋升 / 降级判据。
///
/// `view` 给出该 symbol 的全部事实：
/// - `view.paper` / `view.live`：[`hft_engine_rs::engine::SymbolRecord`]
///   - `.round_trips()` —— **逐次往返的净盈亏**（已扣手续费），做统计就用它
///   - `.realized_pnl()` —— 累计已实现盈亏
///   - `.stats` —— 累计笔数 / 名义额 / 手续费
///   - `.position()` / `.is_flat()` —— 当前是否持仓
/// - `view.is_live()` / `view.live_elapsed_ms()` —— 实盘是否已开、开了多久
/// - `view.now` —— 当前时刻（毫秒）
///
/// 判据可以自己攒状态（本结构体的字段），框架保证同一 symbol 串行调用。
///
/// 返回与当前状态矛盾的决定（未开实盘却 `Demote`、已开却 `Promote`）会被框架忽略并告警。
struct MyPolicy {
    // 需要跨调用记住的东西放这里，例如"信号首次出现的时刻"（用于样本外确认的冷却期）
    _reserved: (),
}

impl PromotionPolicy for MyPolicy {
    fn decide(&mut self, view: &SymbolView<'_>) -> Decision {
        // ===================== 在此实现你的判据 =====================
        //
        // 提醒（详见 engine::PromotionPolicy 的模块文档）：
        //  1. 同时有 N 个 symbol 在跑，即使策略无 edge，最好的那几个几乎必然看起来赚钱。
        //     以"盈利 > 0"晋升会系统性地在幸运跑完之后进场；叠加"亏损即降级"会形成
        //     高位晋升、低位降级的负 alpha 循环。需要最少往返笔数 + 显著性，必要时加
        //     样本外确认。
        //  2. 模拟成交偏乐观（本地柜台无队列位置模型），成交价对但成交机会偏多，门槛留余量。
        //
        // 现状：恒为 Hold —— 只跑模拟、只积累样本，永不真实下单。
        let _ = view;
        Decision::Hold
    }
}

// ============================================================================
// 以下为已接好的框架部分
// ============================================================================

#[derive(Debug, Clone, Deserialize)]
struct ExchangesConfig {
    binance: Option<ExchangeAccess<BinanceCredentials>>,
    okx: Option<ExchangeAccess<OkxCredentials>>,
    hyperliquid: Option<ExchangeAccess<HyperliquidCredentials>>,
}

#[derive(Debug, Clone, Deserialize)]
struct AdaptiveConfig {
    exchanges: ExchangesConfig,
    paper: SimConfig,
    /// 按 24h 成交额取前 N 个 symbol
    top_n: usize,
    /// 挂单相对买价的下偏幅度 (bp)
    offset_bp: f64,
    /// 单笔下单名义额 (USDT)
    order_notional: f64,
}

/// 占位策略：空仓且无挂单时在买价下方挂一张 PostOnly 买单。
///
/// **不是生产策略** —— 只为把链路跑通、让判据有真实样本可看。写好盘口挂单策略后替换掉它，
/// 注意模拟与实盘必须是同一份逻辑同一组参数（由同一个工厂产出，见 `strategy_factory`）。
struct DipMaker {
    symbol: Symbol,
    offset_bp: f64,
    order_notional: f64,
}

impl Strategy for DipMaker {
    fn public_streams(&self) -> HashMap<Exchange, HashSet<SubscriptionKind>> {
        HashMap::from([(
            EXCHANGE,
            HashSet::from([
                SubscriptionKind::BBO {
                    symbol: self.symbol.clone(),
                },
                SubscriptionKind::Trades {
                    symbol: self.symbol.clone(),
                },
            ]),
        )])
    }

    fn order_timeout_ms(&self) -> u64 {
        10_000
    }

    fn on_event(&mut self, event: &IncomeEvent, state: &StateManager) -> Vec<OutcomeEvent> {
        let IncomeEvent::Market(MarketEvent {
            data: MarketData::BBO(bbo),
            ..
        }) = event
        else {
            return Vec::new();
        };
        let Some(symbol_state) = state.symbol_state(&bbo.symbol) else {
            return Vec::new();
        };
        if symbol_state.position_size(EXCHANGE).abs() > 0.0
            || symbol_state.pending_orders().next().is_some()
        {
            return Vec::new();
        }
        let price = bbo.bid_price * (1.0 - self.offset_bp / 10_000.0);
        vec![OutcomeEvent::PlaceOrders {
            orders: vec![Order {
                id: String::new(),
                exchange: EXCHANGE,
                symbol: bbo.symbol.clone(),
                side: Side::Long,
                order_type: OrderType::Limit {
                    price,
                    tif: TimeInForce::PostOnly,
                },
                quantity: self.order_notional / price,
                reduce_only: false,
                client_order_id: String::new(),
            }],
            comment: format!("dip_buy | bid={} | offset={}bp", bbo.bid_price, self.offset_bp),
        }]
    }
}

/// 按 24h 成交额取前 `top_n` 个可交易 symbol。
///
/// 与 symbol metas 取交集，排除 24h 行情里那些**不可交易**的条目（已停牌/交割品种等）；
/// 低流动性 symbol 的盘口很差，在其上跑出的模拟结论本身不可信，所以按成交额筛而非随机取。
async fn top_symbols_by_volume(
    client: &BinanceClient,
    top_n: usize,
) -> anyhow::Result<Vec<Symbol>> {
    let tradable: HashSet<Symbol> = client
        .fetch_all_symbol_metas()
        .await
        .context("拉取 symbol metas 失败")?
        .into_iter()
        .map(|m| m.symbol)
        .collect();

    let mut volumes = client
        .fetch_quote_volumes()
        .await
        .context("拉取 24h 成交额失败")?;
    volumes.retain(|(s, v)| tradable.contains(s) && *v > 0.0);
    // 成交额降序；同额时按 symbol 名排序，保证同一输入的选择是确定的
    volumes.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    volumes.truncate(top_n);

    for (symbol, volume) in &volumes {
        tracing::info!(%symbol, quote_volume_24h = volume, "selected symbol");
    }
    Ok(volumes.into_iter().map(|(s, _)| s).collect())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing()?;
    let config: AdaptiveConfig = load_config("adaptive.json")?;
    anyhow::ensure!(config.top_n > 0, "top_n 必须 > 0");

    let access = config
        .exchanges
        .binance
        .clone()
        .context("本入口目前只支持 Binance，请配置 exchanges.binance")?;

    // 选标的用公共 REST，不需要凭证
    let picker = BinanceClient::new(access.quote.clone(), None)
        .map_err(|e| anyhow::anyhow!("创建 Binance client 失败: {e}"))?;
    let symbols = top_symbols_by_volume(&picker, config.top_n).await?;
    anyhow::ensure!(!symbols.is_empty(), "没有选出任何 symbol");

    let has_credentials = access.credentials.is_some();
    tracing::warn!(
        symbols = ?symbols,
        has_credentials,
        initial_balance = config.paper.initial_balance_usdt,
        "启动自适应晋升：每个 symbol 常驻模拟账户；实盘仅在判据晋升时开启"
    );
    if !has_credentials {
        tracing::warn!(
            "未配置凭证 —— 即使判据返回 Promote 也无法真实下单。只跑模拟时这是期望状态。"
        );
    }

    let manager = ManagerActor::spawn_with_mailbox(
        ManagerActorArgs {
            binance: config.exchanges.binance.clone(),
            okx: config.exchanges.okx.clone(),
            hyperliquid: config.exchanges.hyperliquid.clone(),
            ibkr_credentials: None,
            ibkr_snapshot: None,
            paper: config.paper,
        },
        mailbox::unbounded(),
    );
    manager
        .wait_for_startup_result()
        .await
        .context("ManagerActor 启动失败")?;

    // 模拟与实盘共用同一个工厂：同一逻辑、同一参数，否则模拟结论无法外推到实盘
    let offset_bp = config.offset_bp;
    let order_notional = config.order_notional;
    let strategy_factory = Box::new(move |symbol: &Symbol| {
        Box::new(DipMaker {
            symbol: symbol.clone(),
            offset_bp,
            order_notional,
        }) as Box<dyn Strategy>
    });

    // 挂进引擎的监督树：它出错要整机退出，而不是留下一个没人看着的第二个根
    let supervisor = spawn_supervised::<SupervisorActor>(
        &manager,
        SupervisorArgs {
            symbols: symbols.clone(),
            exchange: EXCHANGE,
            strategy_factory,
            policy: Box::new(MyPolicy { _reserved: () }),
            manager: manager.clone(),
        },
    )
    .await
    .context("SupervisorActor 启动失败")?;

    // 行情总线给 Clock 节拍；账户总线一次订阅覆盖实盘与全部模拟账户的成交
    // （账户由事件自带标签区分，见 AccountEvent）
    manager
        .tell(SubscribeMarket::all(supervisor.clone()))
        .send()
        .await
        .context("订阅行情总线失败")?;
    manager
        .tell(SubscribeAccount::all(supervisor))
        .send()
        .await
        .context("订阅账户总线失败")?;

    // 停机信号 → Ok；manager 意外终止 → Err，进程带着原因非零退出
    wait_for_shutdown(manager).await
}
