use crate::domain::models::Exchange;
use thiserror::Error;

/// 订单被交易所拒绝的结构化原因
///
/// 由各交易所适配层在错误边界按该所的错误码（没有错误码的所按它固定的报错原文）翻译成具体
/// 变体，编排层按枚举判断，不做字符串嗅探。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectReason {
    /// reduce-only 单因仓位已平而被拒——通常无害（平仓目标已达成）
    ReduceOnlyClosed,
    /// 其它拒绝原因，保留原始交易所消息
    Other(String),
}

impl std::fmt::Display for RejectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RejectReason::ReduceOnlyClosed => {
                write!(f, "reduce-only rejected (position already closed)")
            }
            RejectReason::Other(m) => write!(f, "{m}"),
        }
    }
}

/// 下单失败的结论。下单只有三种结果：成功（`Ok`）、被拒绝、错误（结果不确定）。
///
/// 两种失败的处置完全不同，所以各所适配层**必须**为每条失败路径明确选一种 —— 这里有意
/// 不提供从 [`ExchangeError`] 的自动转换，`?` 写不出来，漏选会编译失败：
/// - 被拒绝：单子确定没挂上，策略可以照常再下；
/// - 错误：单子可能已挂上甚至已成交。若当成没挂上，策略会再挂一张、再补一次腿，敞口翻倍。
///   框架对它的处置见 `OrderGateway::place`（交易所回报流已认领则以回报为准，否则停机）。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PlaceError {
    /// 确定没挂上：交易所明确说不接这张单（价格/数量不合规、余额不足、只挂 maker 的单会
    /// 立刻成交、限频……），或请求根本没发出去（本地组装、签名失败）。
    #[error("rejected: {0}")]
    Rejected(RejectReason),
    /// 结果不确定：超时、5xx、网络断开、回复解析不了、交易所自己说"状态未知"的错误码……
    #[error("outcome unknown: {0}")]
    Indeterminate(String),
}

impl PlaceError {
    /// 交易所明确拒单，原因不需要细分（需要细分的由适配层按错误码直接构造 [`RejectReason`]）
    pub fn rejected(msg: impl Into<String>) -> Self {
        PlaceError::Rejected(RejectReason::Other(msg.into()))
    }

    /// 请求没有发出去（本地组装、签名、缺凭证）—— 确定没挂上
    pub fn unsent(e: impl std::fmt::Display) -> Self {
        PlaceError::Rejected(RejectReason::Other(format!("not sent: {e}")))
    }

    /// 结果不确定
    pub fn indeterminate(e: impl std::fmt::Display) -> Self {
        PlaceError::Indeterminate(e.to_string())
    }
}

/// 和交易所打交道（REST、WS、解析回报）失败。
///
/// 只有一种：调用方对任何失败的处置都一样（启动期拒绝启动、运行期上报并停机、轮询记一次失败
/// 下拍再试），没有按失败种类分支的地方，所以不分变体（#813：原先 10 种 + WS 另 5 种，没有一处
/// 分别处理，还为了凑变体编出过「余额不足 need 0 have 0」）。要分清的只有下单结论，那是
/// [`PlaceError`]。原因文字写清哪个所、在做什么、交易所原话。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{0}")]
pub struct ExchangeError(String);

impl ExchangeError {
    pub fn new(msg: impl std::fmt::Display) -> Self {
        Self(msg.to_string())
    }

    /// 没拿到回复：连不上、超时、连接中断、读回复失败
    pub fn transport(exchange: Exchange, e: impl std::fmt::Display) -> Self {
        Self(format!("{exchange} 请求失败: {e}"))
    }

    /// 交易所回了错误：HTTP 状态或交易所错误码 + 原文
    pub fn api(
        exchange: Exchange,
        code: impl std::fmt::Display,
        msg: impl std::fmt::Display,
    ) -> Self {
        Self(format!("{exchange} 返回错误 code={code}: {msg}"))
    }
}

/// codec 的解析函数返回 `Result<_, String>`，在适配层用 `?` 直接带上来
impl From<String> for ExchangeError {
    fn from(msg: String) -> Self {
        Self(msg)
    }
}

impl From<serde_json::Error> for ExchangeError {
    fn from(e: serde_json::Error) -> Self {
        Self(format!("JSON: {e}"))
    }
}
