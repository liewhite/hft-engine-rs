//! ManagerActor - 顶层 Actor，管理所有子 Actor 的生命周期
//!
//! 职责：
//! - 创建 PubSub Actors (IncomePubSub, OutcomePubSub)
//! - 使用 spawn_link 创建所有子 Actor
//! - 通过 add_strategy 动态添加策略和相关 Actor
//! - 子 Actor 失败时级联退出

use super::{
    ClockActor, ClockActorArgs, ExecutorActor, ExecutorArgs, IncomePubSub,
    IncomeProcessorActor, OutcomePubSub, OutcomeProcessorActor, RegisterExecutor,
    SignalProcessorArgs,
};
use crate::domain::{Exchange, ExchangeError, Symbol, SymbolMeta};
use crate::exchange::binance::{
    BinanceActor, BinanceActorArgs, BinanceClient, BinanceCredentials, REST_BASE_URL,
};
use crate::exchange::hyperliquid::{
    HyperliquidActor, HyperliquidActorArgs, HyperliquidClient, HyperliquidCredentials,
};
use crate::exchange::ibkr::{IbkrActor, IbkrActorArgs, IbkrClient, IbkrCredentials};
use crate::exchange::okx::{OkxActor, OkxActorArgs, OkxClient, OkxCredentials};
use crate::exchange::{ExchangeActorOps, ExchangeClient, SubscriptionKind};
use crate::strategy::Strategy;
use kameo::actor::{ActorId, ActorRef, Spawn, WeakActorRef};
use kameo::error::ActorStopReason;
use kameo::mailbox;
use kameo::message::{Context, Message};
use kameo::Actor;
use kameo_actors::pubsub::Subscribe;
use kameo_actors::DeliveryStrategy;
use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::sync::Arc;

/// ManagerActor 初始化参数
pub struct ManagerActorArgs {
    /// Binance 凭证（可选）
    pub binance_credentials: Option<BinanceCredentials>,
    /// OKX 凭证（可选）
    pub okx_credentials: Option<OkxCredentials>,
    /// Hyperliquid 凭证（可选）
    pub hyperliquid_credentials: Option<HyperliquidCredentials>,
    /// IBKR 凭证（可选）
    pub ibkr_credentials: Option<IbkrCredentials>,
}

/// ManagerActor - 顶层管理 Actor
pub struct ManagerActor {
    // === Symbol Metas 缓存 ===
    symbol_metas: HashMap<(Exchange, Symbol), SymbolMeta>,

    // === PubSub Actors ===
    /// Income PubSub (行情/账户事件)
    income_pubsub: ActorRef<IncomePubSub>,
    /// Outcome PubSub (策略信号)
    outcome_pubsub: ActorRef<OutcomePubSub>,

    // === 子 Actors ===
    /// ProcessorActor (订阅 income_pubsub)
    income_processor: ActorRef<IncomeProcessorActor>,
    /// ExchangeActors (启动时创建，类型擦除)
    exchange_actors: HashMap<Exchange, Box<dyn ExchangeActorOps>>,
}

impl ManagerActor {
    /// 预加载所有交易所的 symbol metas（静态方法）
    async fn preload_all_symbol_metas(
        clients: &HashMap<Exchange, Arc<dyn ExchangeClient>>,
    ) -> Result<HashMap<(Exchange, Symbol), SymbolMeta>, ExchangeError> {
        let mut symbol_metas = HashMap::new();

        for (exchange, client) in clients {
            let metas = client.fetch_all_symbol_metas().await?;
            let count = metas.len();
            for meta in metas {
                symbol_metas.insert((*exchange, meta.symbol.clone()), meta);
            }
            tracing::info!(%exchange, count, "Preloaded symbol metas");
        }

        Ok(symbol_metas)
    }

    /// 提取指定交易所的 symbol metas（静态方法）
    fn get_symbol_metas_for(
        symbol_metas: &HashMap<(Exchange, Symbol), SymbolMeta>,
        exchange: Exchange,
    ) -> Arc<HashMap<Symbol, SymbolMeta>> {
        Arc::new(
            symbol_metas
                .iter()
                .filter(|((e, _), _)| *e == exchange)
                .map(|((_, s), m)| (s.clone(), m.clone()))
                .collect(),
        )
    }

    /// 批量添加策略的内部实现
    async fn do_add_strategies(
        &mut self,
        strategies: Vec<Box<dyn Strategy>>,
        actor_ref: ActorRef<Self>,
    ) -> Result<(), ExchangeError> {
        if strategies.is_empty() {
            return Ok(());
        }

        let processor = self.income_processor.clone();
        let outcome_pubsub = self.outcome_pubsub.clone();

        // 1. 收集所有策略的订阅，并按交易所分组
        let mut all_subscriptions: HashSet<(Exchange, SubscriptionKind)> = HashSet::new();
        let mut strategy_subscriptions: Vec<HashSet<(Exchange, SubscriptionKind)>> = Vec::new();

        for strategy in &strategies {
            let public_streams = strategy.public_streams();
            let subscriptions: HashSet<(Exchange, SubscriptionKind)> = public_streams
                .iter()
                .flat_map(|(exchange, kinds)| {
                    kinds.iter().map(move |kind| (*exchange, kind.clone()))
                })
                .collect();
            all_subscriptions.extend(subscriptions.clone());
            strategy_subscriptions.push(subscriptions);
        }

        // 2. 按交易所分组订阅
        let mut exchange_subscriptions: HashMap<Exchange, Vec<SubscriptionKind>> = HashMap::new();
        for (exchange, kind) in &all_subscriptions {
            exchange_subscriptions
                .entry(*exchange)
                .or_default()
                .push(kind.clone());
        }

        // 3. 批量创建 ExecutorActors
        let mut executor_refs = Vec::new();
        for strategy in strategies {
            let executor_ref = ExecutorActor::spawn_link_with_mailbox(
                &actor_ref,
                ExecutorArgs {
                    strategy,
                    symbol_metas: Arc::new(self.symbol_metas.clone()),
                    outcome_pubsub: outcome_pubsub.clone(),
                },
                mailbox::unbounded(),
            )
            .await;
            executor_refs.push(executor_ref);
        }

        // 4. 向 ProcessorActor 注册所有 Executor 的订阅
        for (executor_ref, subscriptions) in executor_refs.iter().zip(strategy_subscriptions.iter())
        {
            let _ = processor
                .tell(RegisterExecutor {
                    executor: executor_ref.clone(),
                    subscriptions: subscriptions.clone(),
                })
                .send()
                .await;
        }

        // 5. 批量向各 ExchangeActors 发送订阅请求
        for (exchange, kinds) in exchange_subscriptions {
            if let Some(actor) = self.exchange_actors.get(&exchange) {
                actor
                    .subscribe_batch(kinds)
                    .await
                    .map_err(ExchangeError::Other)?;
            }
        }

        tracing::info!(
            count = executor_refs.len(),
            "Strategies batch added, ExecutorActors created"
        );

        Ok(())
    }
}

impl Actor for ManagerActor {
    type Args = ManagerActorArgs;
    type Error = ExchangeError;

    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        // 1. 创建 Exchange Clients
        let mut clients: HashMap<Exchange, Arc<dyn ExchangeClient>> = HashMap::new();

        if let Some(ref cred) = args.binance_credentials {
            let client = BinanceClient::new(Some(cred.clone()))?;
            clients.insert(Exchange::Binance, Arc::new(client));
        }

        if let Some(ref cred) = args.okx_credentials {
            let client = OkxClient::new(Some(cred.clone()))?;
            clients.insert(Exchange::OKX, Arc::new(client));
        }

        if let Some(ref cred) = args.hyperliquid_credentials {
            let client = HyperliquidClient::new(Some(cred.clone()))?;
            clients.insert(Exchange::Hyperliquid, Arc::new(client));
        }

        // IBKR 需要额外保存 oauth 和 conids 供 Actor 使用
        let mut ibkr_actor_data = None;
        if let Some(ref cred) = args.ibkr_credentials {
            let client = IbkrClient::new(cred).await?;
            ibkr_actor_data = Some((client.oauth(), client.conids().clone()));
            clients.insert(Exchange::IBKR, Arc::new(client));
        }

        // 2. 预加载所有交易所的 symbol metas
        let symbol_metas = Self::preload_all_symbol_metas(&clients).await?;

        // 3. 创建 PubSub Actors (使用 spawn_link_with_mailbox)
        let income_pubsub = IncomePubSub::spawn_link_with_mailbox(
            &actor_ref,
            IncomePubSub::new(DeliveryStrategy::BestEffort),
            mailbox::unbounded(),
        )
        .await;
        let outcome_pubsub = OutcomePubSub::spawn_link_with_mailbox(
            &actor_ref,
            OutcomePubSub::new(DeliveryStrategy::BestEffort),
            mailbox::unbounded(),
        )
        .await;

        // 4. 创建 ProcessorActor 并订阅 income_pubsub
        let processor = IncomeProcessorActor::spawn_link_with_mailbox(
            &actor_ref,
            IncomeProcessorActor::default(),
            mailbox::unbounded(),
        )
        .await;
        income_pubsub
            .tell(Subscribe(processor.clone()))
            .send()
            .await
            .map_err(|e| ExchangeError::Other(e.to_string()))?;

        // 5. 创建 SignalProcessorActor 并订阅 outcome_pubsub
        let _signal_processor = OutcomeProcessorActor::spawn_link_with_mailbox(
            &actor_ref,
            SignalProcessorArgs {
                clients: clients.clone(),
                income_pubsub: income_pubsub.clone(),
            },
            mailbox::unbounded(),
        )
        .await;
        outcome_pubsub
            .tell(Subscribe(_signal_processor.clone()))
            .send()
            .await
            .map_err(|e| ExchangeError::Other(e.to_string()))?;

        // 6. 创建 ClockActor（发布 Clock 事件到 income_pubsub）
        let _clock_actor = ClockActor::spawn_link_with_mailbox(
            &actor_ref,
            ClockActorArgs {
                interval_ms: 1000,
                income_pubsub: income_pubsub.clone(),
            },
            mailbox::unbounded(),
        )
        .await;

        // 7. 创建所有配置了凭证的 ExchangeActors
        let mut exchange_actors: HashMap<Exchange, Box<dyn ExchangeActorOps>> = HashMap::new();

        if let Some(ref credentials) = args.binance_credentials {
            let symbol_metas_for_exchange =
                Self::get_symbol_metas_for(&symbol_metas, Exchange::Binance);
            let client = clients
                .get(&Exchange::Binance)
                .ok_or_else(|| ExchangeError::Other("Binance client not found".to_string()))?
                .clone();
            let binance_ref = BinanceActor::spawn_link_with_mailbox(
                &actor_ref,
                BinanceActorArgs {
                    credentials: Some(credentials.clone()),
                    symbol_metas: symbol_metas_for_exchange,
                    rest_base_url: REST_BASE_URL.to_string(),
                    income_pubsub: income_pubsub.clone(),
                    client,
                    quote: credentials.quote.clone(),
                },
                mailbox::unbounded(),
            )
            .await;
            exchange_actors.insert(Exchange::Binance, Box::new(binance_ref));
            tracing::info!(exchange = "Binance", "ExchangeActor created");
        }

        if let Some(ref credentials) = args.okx_credentials {
            let symbol_metas_for_exchange = Self::get_symbol_metas_for(&symbol_metas, Exchange::OKX);
            let okx_ref = OkxActor::spawn_link_with_mailbox(
                &actor_ref,
                OkxActorArgs {
                    credentials: Some(credentials.clone()),
                    symbol_metas: symbol_metas_for_exchange,
                    income_pubsub: income_pubsub.clone(),
                    quote: credentials.quote.clone(),
                },
                mailbox::unbounded(),
            )
            .await;
            exchange_actors.insert(Exchange::OKX, Box::new(okx_ref));
            tracing::info!(exchange = "OKX", "ExchangeActor created");
        }

        if let Some(ref credentials) = args.hyperliquid_credentials {
            let symbol_metas_for_exchange =
                Self::get_symbol_metas_for(&symbol_metas, Exchange::Hyperliquid);
            let hyper_ref = HyperliquidActor::spawn_link_with_mailbox(
                &actor_ref,
                HyperliquidActorArgs {
                    credentials: Some(credentials.clone()),
                    symbol_metas: symbol_metas_for_exchange,
                    income_pubsub: income_pubsub.clone(),
                    quote: credentials.quote.clone(),
                },
                mailbox::unbounded(),
            )
            .await;
            exchange_actors.insert(Exchange::Hyperliquid, Box::new(hyper_ref));
            tracing::info!(exchange = "Hyperliquid", "ExchangeActor created");
        }

        if let Some((ibkr_oauth, ibkr_conids)) = ibkr_actor_data {
            let ibkr_ref = IbkrActor::spawn_link_with_mailbox(
                &actor_ref,
                IbkrActorArgs {
                    oauth: ibkr_oauth,
                    income_pubsub: income_pubsub.clone(),
                    conids: ibkr_conids,
                },
                mailbox::unbounded(),
            )
            .await;
            exchange_actors.insert(Exchange::IBKR, Box::new(ibkr_ref));
            tracing::info!(exchange = "IBKR", "ExchangeActor created");
        }

        tracing::info!("ManagerActor started with all child actors linked");

        Ok(Self {
            symbol_metas,
            income_pubsub,
            outcome_pubsub,
            income_processor: processor,
            exchange_actors,
        })
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        reason: ActorStopReason,
    ) -> Result<(), Self::Error> {
        tracing::info!(reason = ?reason, "ManagerActor stopped");
        Ok(())
    }

    async fn on_link_died(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        id: ActorId,
        reason: ActorStopReason,
    ) -> Result<ControlFlow<ActorStopReason>, Self::Error> {
        tracing::error!(actor_id = ?id, reason = ?reason, "Child actor died, shutting down");
        Ok(ControlFlow::Break(ActorStopReason::LinkDied {
            id,
            reason: Box::new(reason),
        }))
    }
}

// ============================================================================
// Messages
// ============================================================================

/// 添加策略
pub struct AddStrategy(pub Box<dyn Strategy>);

impl Message<AddStrategy> for ManagerActor {
    type Reply = Result<(), ExchangeError>;

    async fn handle(
        &mut self,
        msg: AddStrategy,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        // 委托给批量添加
        self.do_add_strategies(vec![msg.0], ctx.actor_ref().clone())
            .await
    }
}

/// 批量添加策略
pub struct AddStrategies(pub Vec<Box<dyn Strategy>>);

impl Message<AddStrategies> for ManagerActor {
    type Reply = Result<(), ExchangeError>;

    async fn handle(
        &mut self,
        msg: AddStrategies,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.do_add_strategies(msg.0, ctx.actor_ref().clone()).await
    }
}

/// 停止管理器
pub struct Stop;

impl Message<Stop> for ManagerActor {
    type Reply = ();

    async fn handle(&mut self, _msg: Stop, ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        tracing::info!("Stopping ManagerActor...");
        ctx.actor_ref().stop_gracefully().await.ok();
    }
}

/// 订阅 Income 事件
pub struct SubscribeIncome<A: Actor>(pub ActorRef<A>);

impl<A> Message<SubscribeIncome<A>> for ManagerActor
where
    A: Actor + Message<crate::messaging::IncomeEvent>,
{
    type Reply = ();

    async fn handle(
        &mut self,
        msg: SubscribeIncome<A>,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let _ = self.income_pubsub.tell(Subscribe(msg.0)).send().await;
    }
}

/// 订阅 Outcome 事件
pub struct SubscribeOutcome<A: Actor>(pub ActorRef<A>);

impl<A> Message<SubscribeOutcome<A>> for ManagerActor
where
    A: Actor + Message<crate::strategy::OutcomeEvent>,
{
    type Reply = ();

    async fn handle(
        &mut self,
        msg: SubscribeOutcome<A>,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let _ = self.outcome_pubsub.tell(Subscribe(msg.0)).send().await;
    }
}

/// 获取所有交易所的 SymbolMeta
pub struct GetAllSymbolMetas;

impl Message<GetAllSymbolMetas> for ManagerActor {
    type Reply = HashMap<Exchange, Vec<SymbolMeta>>;

    async fn handle(
        &mut self,
        _msg: GetAllSymbolMetas,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let mut result: HashMap<Exchange, Vec<SymbolMeta>> = HashMap::new();
        for ((exchange, _), meta) in &self.symbol_metas {
            result.entry(*exchange).or_default().push(meta.clone());
        }
        result
    }
}
