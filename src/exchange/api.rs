use crate::domain::{Exchange, ExchangeError, Order, OrderId, Symbol, SymbolMeta};
use async_trait::async_trait;

/// 公共数据类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PublicDataType {
    /// 资金费率
    FundingRate,
    /// Best Bid/Offer
    BBO,
}

impl PublicDataType {
    /// 返回所有数据类型
    pub fn all() -> &'static [PublicDataType] {
        &[PublicDataType::FundingRate, PublicDataType::BBO]
    }
}

/// 交易所执行器 trait
#[async_trait]
pub trait ExchangeExecutor: Send + Sync {
    fn exchange(&self) -> Exchange;

    /// 获取交易对元数据
    async fn fetch_symbol_meta(&self, symbols: &[Symbol]) -> Result<Vec<SymbolMeta>, ExchangeError>;

    /// 下单
    async fn place_order(&self, order: Order) -> Result<OrderId, ExchangeError>;

    /// 设置杠杆
    async fn set_leverage(&self, symbol: &Symbol, leverage: u32) -> Result<(), ExchangeError>;

    /// 获取账户净值 (balance + unrealized_pnl)
    async fn fetch_equity(&self) -> Result<f64, ExchangeError>;
}
