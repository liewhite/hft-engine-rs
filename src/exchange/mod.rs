pub mod binance;
pub mod client;
pub mod client_order_id;
pub mod hyperliquid;
pub mod ibkr;
pub mod okx;
pub mod crypto_status;
pub(crate) mod equity_polling;
pub(crate) mod funding_fee_polling;
pub(crate) mod lazy_conn;
pub mod staleness;
pub mod ws_loop;

/// trades 线路一致性测试（联网，`#[ignore]`）
#[cfg(test)]
mod trades_conformance;

pub use client_order_id::CliOrderIdFormat;
pub use crypto_status::CryptoStatus;
pub use client::{
    AccountClient, AccountStream, ExchangeAccess, ExchangeClient, ExchangeCmd, ExchangeHandle, ExchangeOrder,
    FundingFeeSource, SettledFundingSource, SubscriptionKind,
};
