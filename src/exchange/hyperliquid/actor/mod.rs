mod hyperliquid_actor;
mod public_ws;

pub use hyperliquid_actor::{HyperliquidActor, HyperliquidActorArgs};

/// 内部 WebSocket 数据消息
pub struct WsData {
    pub data: String,
}
