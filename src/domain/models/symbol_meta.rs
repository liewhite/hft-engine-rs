use crate::domain::models::{Exchange, Symbol};
use crate::exchange::utils::PriceFormatter;
use rust_decimal::prelude::*;
use rust_decimal::Decimal;
use std::sync::Arc;

/// 交易对元数据
#[derive(Debug, Clone)]
pub struct SymbolMeta {
    pub exchange: Exchange,
    pub symbol: Symbol,
    /// 价格格式化器
    pub price_formatter: Arc<dyn PriceFormatter>,
    /// 数量精度 (最小数量变动单位)
    pub size_step: f64,
    /// 最小下单数量
    pub min_order_size: f64,
    /// 合约乘数 (OKX: cval, Binance: 1.0)
    ///
    /// 表示每张合约对应的币本位数量
    /// - OKX ETH: cval=0.1, 下单 qty=1 表示 0.1 ETH
    /// - Binance: 直接按币的数量下单, 等效于 cval=1.0
    pub contract_size: f64,
}

impl SymbolMeta {
    /// 检查元数据是否有效
    pub fn is_valid(&self) -> bool {
        self.size_step > 0.0 && self.contract_size > 0.0
    }

    /// 将币本位数量转换为下单数量
    ///
    /// 例如: 想下 0.5 ETH, OKX cval=0.1, 则返回 5 (张)
    pub fn coin_to_qty(&self, coin_amount: f64) -> f64 {
        coin_amount / self.contract_size
    }

    /// 将下单数量转换为币本位数量
    ///
    /// 例如: 下单 5 张, OKX cval=0.1, 则返回 0.5 ETH
    pub fn qty_to_coin(&self, qty: f64) -> f64 {
        qty * self.contract_size
    }

    /// 格式化价格为字符串 (用于 API 请求)
    pub fn format_price(&self, price: f64) -> String {
        self.price_formatter.format(price)
    }

    /// 将价格取整到合法精度 (通过 format 再 parse)
    pub fn round_price(&self, price: f64) -> f64 {
        self.price_formatter
            .format(price)
            .parse()
            .unwrap_or(price)
    }

    /// 将数量调整到合法精度 (向下取整)
    pub fn round_size_down(&self, size: f64) -> f64 {
        Self::round_to_step(size, self.size_step, RoundingStrategy::ToNegativeInfinity)
    }

    /// 使用 Decimal 精确计算，按 step 取整
    fn round_to_step(value: f64, step: f64, strategy: RoundingStrategy) -> f64 {
        let value_dec = Decimal::from_f64(value).unwrap_or_default();
        let step_dec = Decimal::from_f64(step).unwrap_or(Decimal::ONE);

        let ticks = value_dec / step_dec;
        let rounded_ticks = ticks.round_dp_with_strategy(0, strategy);

        let result = rounded_ticks * step_dec;
        result.to_f64().unwrap_or(value)
    }
}
