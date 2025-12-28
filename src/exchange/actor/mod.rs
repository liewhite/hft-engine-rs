//! Exchange Actor 模块
//!
//! - PublicWsActor: 管理单个公共 WebSocket 连接
//! - BinancePrivateWsActor: Binance 私有 WebSocket 连接 (ListenKey 认证)
//! - OkxPrivateWsActor: OKX 私有 WebSocket 连接 (Login 认证)
//! - ExchangeActor: 管理单个交易所的所有 WebSocket 连接

mod binance_private_ws;
mod exchange;
mod okx_private_ws;
pub mod private_ws;
pub mod public_ws;

pub use binance_private_ws::{BinancePrivateHandle, BinancePrivateWsActor, BinancePrivateWsActorArgs};
pub use exchange::{ExchangeActor, ExchangeActorArgs, MarketDataSink};
pub use okx_private_ws::{OkxPrivateHandle, OkxPrivateWsActor, OkxPrivateWsActorArgs};
pub use private_ws::PrivateConnectionHandle;
pub use public_ws::{ConnectionId, PublicWsActor, PublicWsActorArgs, SendMessage, WsData, WsDataSink, WsError};
