/// 账户信息 (净值 + 总持仓名义价值)
///
/// 纯数据载体。定义在 domain 层作为单一数据源，供 exchange 适配层（`ExchangeClient`
/// 的返回类型）与 messaging 层（`StateManager` 缓存）共用，避免消息层反向依赖交易所层。
#[derive(Debug, Clone, Copy)]
pub struct AccountInfo {
    /// 账户净值 (balance + unrealized_pnl)
    pub equity: f64,
    /// 账户总持仓名义价值 (用于计算杠杆率)
    pub notional: f64,
}
