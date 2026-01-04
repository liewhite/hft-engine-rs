//! Binance ExchangeModule 实现

use super::{BinanceActor, BinanceActorArgs, BinanceClient, BinanceCredentials, REST_BASE_URL};
use crate::domain::{Exchange, Symbol, SymbolMeta};
use crate::exchange::{EventSink, ExchangeActorOps, ExchangeClient, ExchangeModule};
use kameo::actor::{spawn_link, ActorID, ActorRef};
use kameo::Actor;
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

    /// 创建并 spawn_link 交易所 Actor（泛型方法，可接受任意 parent Actor）
    pub async fn spawn_actor<P: Actor>(
        &self,
        parent: &ActorRef<P>,
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

impl ExchangeModule for BinanceModule {
    fn exchange(&self) -> Exchange {
        Exchange::Binance
    }

    fn client(&self) -> Arc<dyn ExchangeClient> {
        self.client.clone()
    }
}
