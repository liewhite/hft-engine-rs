pub mod binance;
pub mod client;
pub mod hyperliquid;
pub mod okx;
pub mod ws_loop;

pub use client::{
    EventSink, ExchangeActorOps, ExchangeClient, ExchangeModule, Subscribe, SubscriptionKind,
    Unsubscribe,
};

// 为各交易所 Actor 实现 ExchangeActorOps
crate::impl_exchange_actor_ops!(
    binance::BinanceActor,
    okx::OkxActor,
    hyperliquid::HyperliquidActor
);

// ============================================================================
// AnyModule - 交易所模块的 enum dispatch
// ============================================================================

use crate::domain::{Exchange, Symbol, SymbolMeta};
use kameo::actor::{ActorID, ActorRef};
use kameo::Actor;
use std::collections::HashMap;
use std::sync::Arc;

/// 任意交易所模块（enum dispatch，避免 trait object 限制）
///
/// 使用 enum 实现 spawn_actor 的多态分发，因为泛型方法无法在 trait object 中使用
pub enum AnyModule {
    Binance(binance::BinanceModule),
    Okx(okx::OkxModule),
    Hyperliquid(hyperliquid::HyperliquidModule),
}

impl AnyModule {
    /// 获取交易所标识
    pub fn exchange(&self) -> Exchange {
        match self {
            Self::Binance(m) => m.exchange(),
            Self::Okx(m) => m.exchange(),
            Self::Hyperliquid(m) => m.exchange(),
        }
    }

    /// 获取 REST 客户端
    pub fn client(&self) -> Arc<dyn ExchangeClient> {
        match self {
            Self::Binance(m) => m.client(),
            Self::Okx(m) => m.client(),
            Self::Hyperliquid(m) => m.client(),
        }
    }

    /// 创建并 spawn_link 交易所 Actor
    pub async fn spawn_actor<P: Actor>(
        &self,
        parent: &ActorRef<P>,
        symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
        event_sink: Arc<dyn EventSink>,
    ) -> (ActorID, Box<dyn ExchangeActorOps>) {
        match self {
            Self::Binance(m) => m.spawn_actor(parent, symbol_metas, event_sink).await,
            Self::Okx(m) => m.spawn_actor(parent, symbol_metas, event_sink).await,
            Self::Hyperliquid(m) => m.spawn_actor(parent, symbol_metas, event_sink).await,
        }
    }
}

impl ExchangeModule for AnyModule {
    fn exchange(&self) -> Exchange {
        self.exchange()
    }

    fn client(&self) -> Arc<dyn ExchangeClient> {
        self.client()
    }
}
