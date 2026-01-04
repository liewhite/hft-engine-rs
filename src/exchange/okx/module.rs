//! OKX ExchangeModule 实现

use super::{OkxActor, OkxActorArgs, OkxClient, OkxCredentials};
use crate::domain::{Exchange, Symbol, SymbolMeta};
use crate::exchange::{EventSink, ExchangeActorOps, ExchangeClient, ExchangeModule};
use kameo::actor::{spawn_link, ActorID, ActorRef};
use kameo::Actor;
use std::collections::HashMap;
use std::sync::Arc;

/// OKX 交易所模块
pub struct OkxModule {
    client: Arc<OkxClient>,
}

impl OkxModule {
    /// 创建新的 OkxModule
    pub fn new(credentials: Option<OkxCredentials>) -> Result<Self, crate::domain::ExchangeError> {
        let client = Arc::new(OkxClient::new(credentials)?);
        Ok(Self { client })
    }

    /// 创建并 spawn_link 交易所 Actor（泛型方法，可接受任意 parent Actor）
    pub async fn spawn_actor<P: Actor>(
        &self,
        parent: &ActorRef<P>,
        symbol_metas: Arc<HashMap<Symbol, SymbolMeta>>,
        event_sink: Arc<dyn EventSink>,
    ) -> (ActorID, Box<dyn ExchangeActorOps>) {
        let actor = OkxActor::new(OkxActorArgs {
            credentials: self.client.credentials().cloned(),
            symbol_metas,
            event_sink,
        });
        let actor_ref = spawn_link(parent, actor).await;
        (actor_ref.id(), Box::new(actor_ref))
    }
}

impl ExchangeModule for OkxModule {
    fn exchange(&self) -> Exchange {
        Exchange::OKX
    }

    fn client(&self) -> Arc<dyn ExchangeClient> {
        self.client.clone()
    }
}
