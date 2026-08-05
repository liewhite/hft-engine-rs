use std::fmt;

/// 账户身份 —— 私有事件（订单回报 / 成交 / 持仓 / 净值）的归属。
///
/// # 为什么需要它
///
/// **行情是共享的一份，私有事件不是。** 实盘账户是交易所里那个真实账户；模拟账户在本地柜台，
/// 而且可以同时存在很多个（每个 symbol 一个）。同一 symbol 上完全可能一个模拟账户与实盘账户
/// **并行**（模拟先跑、出信号后拉起实盘），此时两边的成交与持仓必须互不可见 —— 否则策略的
/// `StateManager` 会把对方的成交计进自己的仓位，两套盈亏一起失真。
///
/// 之前用"运行模式"表达这件事（整个进程要么实盘要么模拟），无法表达并行，所以换成账户身份：
/// 每个策略实例绑定一个账户，事件按账户路由。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AccountId {
    /// 交易所里的真实账户。一套凭证即一个账户，多个实盘策略实例共享它。
    Live,
    /// 本地柜台里的模拟账户。`label` 用于区分（本项目按 symbol 分账，以便逐 symbol 统计盈亏）。
    Paper(String),
}

impl AccountId {
    /// 模拟账户的标签；实盘账户返回 `None`
    pub fn paper_label(&self) -> Option<&str> {
        match self {
            AccountId::Live => None,
            AccountId::Paper(label) => Some(label),
        }
    }

    pub fn is_paper(&self) -> bool {
        matches!(self, AccountId::Paper(_))
    }
}

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AccountId::Live => write!(f, "live"),
            AccountId::Paper(label) => write!(f, "paper:{label}"),
        }
    }
}
