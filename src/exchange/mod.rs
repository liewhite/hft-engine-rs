pub mod binance;
pub mod client;
pub mod hyperliquid;
pub mod okx;
pub mod utils;
pub mod ws_loop;

pub use client::{
    ExchangeActorOps, ExchangeClient, Subscribe, SubscriptionKind, Unsubscribe,
};

// 为各交易所 Actor 实现 ExchangeActorOps
crate::impl_exchange_actor_ops!(
    binance::BinanceActor,
    okx::OkxActor,
    hyperliquid::HyperliquidActor
);
