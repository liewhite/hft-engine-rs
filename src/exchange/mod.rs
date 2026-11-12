pub mod binance;
pub mod client;
pub mod hyperliquid;
pub mod ibkr;
pub mod okx;
pub mod crypto_status;
pub(crate) mod equity_polling;
pub mod staleness;
pub mod ws_loop;

/// trades 线路一致性测试（联网，`#[ignore]`）
#[cfg(test)]
mod trades_conformance;

pub use crypto_status::{CryptoStatusActor, CryptoStatusActorArgs};
pub use client::{
    AccountClient, ExchangeAccess, ExchangeActorOps, ExchangeClient, ExchangeOrder, SettledFundingSource,
    Subscribe, SubscribeBatch, SubscriptionKind, Unsubscribe,
};

// 为各交易所 Actor 实现 ExchangeActorOps
crate::impl_exchange_actor_ops!(
    binance::BinanceActor,
    okx::OkxActor,
    hyperliquid::HyperliquidActor,
    ibkr::IbkrActor
);

/// 三所的已结算资金费拉取都是各 client 的固有方法，这里统一挂到 trait 上
macro_rules! impl_settled_funding_source {
    ($($client:ty => $ex:expr),*) => {
        $(
            #[async_trait::async_trait]
            impl SettledFundingSource for $client {
                fn exchange(&self) -> crate::domain::Exchange {
                    $ex
                }
                async fn fetch_settled_funding(
                    &self,
                    symbol: &crate::domain::Symbol,
                    start_ms: crate::domain::Timestamp,
                ) -> Result<Vec<crate::domain::SettledFunding>, crate::domain::ExchangeError> {
                    <$client>::fetch_settled_funding(self, symbol, start_ms).await
                }
            }
        )*
    };
}
impl_settled_funding_source!(
    binance::BinanceClient => crate::domain::Exchange::Binance,
    hyperliquid::HyperliquidClient => crate::domain::Exchange::Hyperliquid,
    okx::OkxClient => crate::domain::Exchange::OKX
);
