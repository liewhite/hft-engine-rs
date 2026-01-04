//! Hyperliquid ExchangeModule 实现

use super::{HyperliquidActor, HyperliquidActorArgs, HyperliquidClient, HyperliquidCredentials};
use crate::domain::{Exchange, Symbol, SymbolMeta};
use crate::exchange::{EventSink, ExchangeActorOps, ExchangeClient, ExchangeModule};
use kameo::actor::{spawn_link, ActorID, ActorRef};
use kameo::Actor;
use std::collections::HashMap;
use std::sync::Arc;

/// Hyperliquid 交易所模块
pub struct HyperliquidModule {
    client: Arc<HyperliquidClient>,
}

impl HyperliquidModule {
    /// 创建新的 HyperliquidModule
    pub fn new(
        credentials: Option<HyperliquidCredentials>,
    ) -> Result<Self, crate::domain::ExchangeError> {
        let client = Arc::new(HyperliquidClient::new(credentials)?);
        Ok(Self { client })
    }

    /// 创建并 spawn_link 交易所 Actor（泛型方法，可接受任意 parent Actor）
    pub async fn spawn_actor<P: Actor>(
        &self,
        parent: &ActorRef<P>,
        symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
        event_sink: Arc<dyn EventSink>,
    ) -> (ActorID, Box<dyn ExchangeActorOps>) {
        let actor = HyperliquidActor::new(HyperliquidActorArgs {
            credentials: self.client.credentials().cloned(),
            symbol_metas,
            event_sink,
        });
        let actor_ref = spawn_link(parent, actor).await;
        (actor_ref.id(), Box::new(actor_ref))
    }
}

impl ExchangeModule for HyperliquidModule {
    fn exchange(&self) -> Exchange {
        Exchange::Hyperliquid
    }

    fn client(&self) -> Arc<dyn ExchangeClient> {
        self.client.clone()
    }
}
