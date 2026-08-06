use crate::domain::models::Exchange;

/// 账户希腊值（按币种）：**只有 delta**。
///
/// `gamma` / `theta` / `vega` 曾经也在这里，零读者：唯一的生产者是 OKX 的 greeks 轮询
/// （6 行 parse + 错误映射），而回测的合成源为了跟它对齐口径还专门写了两处单位换算
/// （theta 每年→每日、vega 对 1.0→对 1%）。删掉字段，这些换算与它们的单位约定文档一起消失。
///
/// `delta` 有真实读者：经现货余额修正后，是 gamma_scalp 判断方向敞口、决定对冲量的依据。
/// 将来要做真正的 gamma scalping（按 gamma 调整对冲频率）时再加回来 —— 那时它会有读者。
#[derive(Debug, Clone)]
pub struct Greeks {
    pub exchange: Exchange,
    /// 币种 (e.g., "BTC", "ETH")
    pub ccy: String,
    /// 方向敞口（调用方按现货余额修正后使用，见 `StateManager::greeks`）
    pub delta: f64,
    /// 交易所给出的读数时刻：生产端据此去重（同一时刻的重复推送不再发布）
    pub timestamp: u64,
}
