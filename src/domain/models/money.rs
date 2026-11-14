/// 一笔带币种的金额：数额 + 币种（交易所原样的名字，如 USDT、USDC、BNB）
///
/// 框架只传事实（#817、#841）：金额一律带上它实际的币种，不假定 1 USDT = 1 USDC、也不换算；
/// 不同币种的数不在框架里相加。要不要按某个汇率合并是策略（或它的看板）的决定。
#[derive(Debug, Clone, PartialEq)]
pub struct Money {
    pub amount: f64,
    pub asset: String,
}

impl Money {
    pub fn new(amount: f64, asset: impl Into<String>) -> Self {
        Self {
            amount,
            asset: asset.into(),
        }
    }
}
