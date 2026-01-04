//! Hyperliquid ExchangeModule 实现

use super::{HyperliquidActor, HyperliquidActorArgs, HyperliquidClient, HyperliquidCredentials};
use crate::domain::{Exchange, Symbol, SymbolMeta};
use crate::engine::live::{ExchangeModule, ManagerActor};
use crate::exchange::{EventSink, ExchangeActorOps, ExchangeClient};
use async_trait::async_trait;
use kameo::actor::{spawn_link, ActorID, ActorRef};
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
}

#[async_trait]
impl ExchangeModule for HyperliquidModule {
    fn exchange(&self) -> Exchange {
        Exchange::Hyperliquid
    }

    fn client(&self) -> Arc<dyn ExchangeClient> {
        self.client.clone()
    }

    async fn spawn_actor(
        &self,
        parent: &ActorRef<ManagerActor>,
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
