use crate::domain::models::Exchange;

/// 账户希腊值（按币种）。
///
/// # 为什么四个都留着，即使当前只有 delta 有代码读者
///
/// 它们是**期权风险的四个基本维度**，不是可选的附加信息：`delta` 方向敞口、`gamma`
/// delta 的变化速度、`theta` 时间衰减、`vega` 隐波敞口。持有期权腿的系统看不到 theta 与
/// vega，等于不知道自己每天亏多少时间价值、隐波跳一格会怎样。
///
/// 而 `gamma` 更是本项目 gamma scalping 策略的收益来源本身（见
/// [`crate::strategy::GammaScalpStrategy`]：波动驱动 delta 漂移、越带 rehedge 赚取波动）。
/// 当前实现是"最小核心形态"（对称对冲、固定间距），按 gamma 调整对冲带宽/频率是自然的
/// 下一步——那时它就有读者了。
///
/// **口径**：`theta` 每日、`vega` 对 1% 隐波变动。这是期权行业惯例，OKX 的 `thetaBS`/
/// `vegaBS` 与回测的 BS 合成源都按此归一（见 [`crate::backtest`] 的合成源）——这是**正确
/// 的跨源对齐**，不是为迁就字段付的成本。
///
/// 曾在 `d0cec70` 以"零读者"为由删掉过 gamma/theta/vega，随即恢复：判据用错了。
/// "当前没人读"对**领域概念的完备维度**不是充分理由；真正该删的是那些"多数交易所给不
/// 出来、只能靠编造填充"的字段（见 `docs/field-audit.md` 的三类理由）。希腊值只有一个
/// 生产者（OKX）且如实提供，不存在跨所口径分歧。
#[derive(Debug, Clone)]
pub struct Greeks {
    pub exchange: Exchange,
    /// 币种 (e.g., "BTC", "ETH")
    pub ccy: String,
    /// 方向敞口（调用方按现货余额修正后使用，见 `StateManager::greeks`）
    pub delta: f64,
    /// delta 对标的价格的变化率：gamma scalping 的收益来源
    pub gamma: f64,
    /// 时间衰减，**每日**口径
    pub theta: f64,
    /// 隐波敞口，对 **1%** 隐波变动
    pub vega: f64,
    /// 交易所给出的读数时刻：生产端据此去重（同一时刻的重复推送不再发布）
    pub timestamp: u64,
}
