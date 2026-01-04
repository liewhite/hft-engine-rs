//! ExchangeModule trait - 交易所模块抽象
//!
//! 每个交易所实现此 trait，封装:
//! - REST 客户端 (ExchangeClient)
//! - Actor 创建逻辑

use super::engine::ManagerActor;
use crate::domain::{Exchange, Symbol, SymbolMeta};
use crate::exchange::{EventSink, ExchangeActorOps, ExchangeClient};
use async_trait::async_trait;
use kameo::actor::{ActorID, ActorRef};
use std::collections::HashMap;
use std::sync::Arc;

/// 交易所模块 trait
///
/// 封装交易所的客户端和 Actor 创建逻辑，使 ManagerActor 无需知道具体交易所类型
#[async_trait]
pub trait ExchangeModule: Send + Sync {
    /// 获取交易所标识
    fn exchange(&self) -> Exchange;

    /// 获取 REST 客户端
    fn client(&self) -> Arc<dyn ExchangeClient>;

    /// 创建并 spawn_link 交易所 Actor
    ///
    /// 返回 (ActorID, Box<dyn ExchangeActorOps>)，供 ManagerActor 管理
    async fn spawn_actor(
        &self,
        parent: &ActorRef<ManagerActor>,
        symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
        event_sink: Arc<dyn EventSink>,
    ) -> (ActorID, Box<dyn ExchangeActorOps>);
}
