//! OKX ExchangeModule 实现

use super::{OkxActor, OkxActorArgs, OkxClient, OkxCredentials};
use crate::domain::{Exchange, Symbol, SymbolMeta};
use crate::engine::live::{ExchangeModule, ManagerActor};
use crate::exchange::{EventSink, ExchangeActorOps, ExchangeClient};
use async_trait::async_trait;
use kameo::actor::{spawn_link, ActorID, ActorRef};
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
}

#[async_trait]
impl ExchangeModule for OkxModule {
    fn exchange(&self) -> Exchange {
        Exchange::OKX
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
        let actor = OkxActor::new(OkxActorArgs {
            credentials: self.client.credentials().cloned(),
            symbol_metas,
            event_sink,
        });
        let actor_ref = spawn_link(parent, actor).await;
        (actor_ref.id(), Box::new(actor_ref))
    }
}
