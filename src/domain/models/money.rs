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

    /// 从交易所成交回报取手续费（数额已按「正数 = 收费」归一）。数额为 0 记作没收手续费（`None`，
    /// 这时交易所给不给币种都不管）；非 0 却没有币种是交易所违约，报错、不猜币种。
    pub fn fee_from_report(amount: f64, asset: Option<&str>) -> Result<Option<Self>, String> {
        if amount == 0.0 {
            return Ok(None);
        }
        match asset.filter(|a| !a.is_empty()) {
            Some(asset) => Ok(Some(Self::new(amount, asset))),
            None => Err(format!("成交手续费 {amount} 没有币种")),
        }
    }
}
