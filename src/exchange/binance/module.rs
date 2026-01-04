//! Binance ExchangeModule 实现

use super::{BinanceActor, BinanceActorArgs, BinanceClient, BinanceCredentials, REST_BASE_URL};
use crate::domain::{Exchange, Symbol, SymbolMeta};
use crate::engine::live::{ExchangeModule, ManagerActor};
use crate::exchange::{EventSink, ExchangeActorOps, ExchangeClient};
use async_trait::async_trait;
use kameo::actor::{spawn_link, ActorID, ActorRef};
use std::collections::HashMap;
use std::sync::Arc;

/// Binance 交易所模块
pub struct BinanceModule {
    client: Arc<BinanceClient>,
}

impl BinanceModule {
    /// 创建新的 BinanceModule
    pub fn new(credentials: Option<BinanceCredentials>) -> Result<Self, crate::domain::ExchangeError> {
        let client = Arc::new(BinanceClient::new(credentials)?);
        Ok(Self { client })
    }
}

#[async_trait]
impl ExchangeModule for BinanceModule {
    fn exchange(&self) -> Exchange {
        Exchange::Binance
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
        let actor = BinanceActor::new(BinanceActorArgs {
            credentials: self.client.credentials().cloned(),
            symbol_metas,
            event_sink,
            rest_base_url: REST_BASE_URL.to_string(),
        });
        let actor_ref = spawn_link(parent, actor).await;
        (actor_ref.id(), Box::new(actor_ref))
    }
}
