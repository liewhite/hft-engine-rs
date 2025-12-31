//! ExecutorActor - 包装 Strategy 的 Actor
//!
//! 接收 ExchangeEvent，调用 Strategy.on_event()

use crate::domain::{Exchange, Symbol, SymbolMeta};
use crate::engine::SignalProcessorActor;
use crate::messaging::{ExchangeEvent, StateManager};
use crate::strategy::Strategy;
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::{ActorStopReason, BoxError};
use kameo::mailbox::unbounded::UnboundedMailbox;
use kameo::message::{Context, Message};
use kameo::Actor;
use std::collections::HashMap;
use std::sync::Arc;

/// ExecutorActor 初始化参数
pub struct ExecutorArgs {
    /// 策略实例
    pub strategy: Box<dyn Strategy>,
    /// Symbol 元数据 (用于 qty 转换)
    pub symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>>,
    /// SignalProcessorActor 引用 (用于下单)
    pub signal_processor: ActorRef<SignalProcessorActor>,
}

/// ExecutorActor - 执行策略的 Actor
pub struct ExecutorActor {
    /// 策略实例
    strategy: Box<dyn Strategy>,
    /// 状态管理器
    state_manager: StateManager,
}

impl ExecutorActor {
    /// 创建 ExecutorActor
    pub fn new(args: ExecutorArgs) -> Self {
        // 从策略获取订阅的 symbols (去重)
        let public_streams = args.strategy.public_streams();
        let symbols: Vec<Symbol> = public_streams
            .values()
            .flat_map(|kinds| kinds.iter().map(|k| k.symbol().clone()))
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        // 创建状态管理器
        let order_timeout_ms = args.strategy.order_timeout_ms();
        let state_manager = StateManager::new(
            &symbols,
            args.symbol_metas,
            args.signal_processor,
            order_timeout_ms,
        );

        Self {
            strategy: args.strategy,
            state_manager,
        }
    }

    /// 处理 ExchangeEvent，调用策略
    fn handle_event(&mut self, event: ExchangeEvent) {
        self.state_manager.apply(&event);
        self.strategy.on_event(&event, &mut self.state_manager);
    }
}

impl Actor for ExecutorActor {
    type Mailbox = UnboundedMailbox<Self>;

    fn name() -> &'static str {
        "ExecutorActor"
    }

    async fn on_start(&mut self, _actor_ref: ActorRef<Self>) -> Result<(), BoxError> {
        tracing::info!("ExecutorActor started");
        Ok(())
    }

    async fn on_stop(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        _reason: ActorStopReason,
    ) -> Result<(), BoxError> {
        tracing::info!("ExecutorActor stopped");
        Ok(())
    }
}

// === Messages ===

/// ExchangeEvent 消息 - 从 ProcessorActor 接收 (包含所有事件类型，含 Clock)
impl Message<ExchangeEvent> for ExecutorActor {
    type Reply = ();

    async fn handle(&mut self, msg: ExchangeEvent, _ctx: Context<'_, Self, Self::Reply>) {
        self.handle_event(msg);
    }
}
