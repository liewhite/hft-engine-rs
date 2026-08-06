use crate::domain::models::Exchange;

/// 币种余额：**只有可用额**。
///
/// 曾经还有 `frozen`（冻结额）与 `total()`。两者都删了：`total()` 零调用方，而 `frozen`
/// 四个所里只有一家如实提供 —— OKX 有 `frozenBal`；**Binance 根本不给这个字段，适配层用
/// `(walletBalance - available).max(0)` 推导**，而全仓账户下这两个值相等，于是它恒为 0；
/// Hyperliquid 写死 0；IBKR 压根不发 `Balance` 事件。一个"三家在编、无人读"的字段。
///
/// `available` 有真实读者：它进 `StateManager` 的币种现金余额，用于修正 Greeks 的 delta
/// （现货持有量要计入方向敞口），再由 gamma_scalp 决定对冲方向与数量。
#[derive(Debug, Clone)]
pub struct Balance {
    pub exchange: Exchange,
    pub asset: String,
    /// 可用余额
    pub available: f64,
}
