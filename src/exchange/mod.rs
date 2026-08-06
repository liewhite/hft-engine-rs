pub mod binance;
pub mod client;
pub mod hyperliquid;
pub mod ibkr;
pub mod okx;
pub mod staleness;
pub mod utils;
pub mod ws_loop;

/// trades 线路一致性测试（联网，`#[ignore]`）
#[cfg(test)]
mod trades_conformance;

pub use client::{
    supports_subscription, ExchangeAccess, ExchangeActorOps, ExchangeClient, ExchangeOrder,
    Subscribe, SubscribeBatch, SubscriptionKind, Unsubscribe,
};

// 为各交易所 Actor 实现 ExchangeActorOps
crate::impl_exchange_actor_ops!(
    binance::BinanceActor,
    okx::OkxActor,
    hyperliquid::HyperliquidActor,
    ibkr::IbkrActor
);
