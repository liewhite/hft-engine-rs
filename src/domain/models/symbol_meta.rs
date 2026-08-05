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

    /// 将**交易所下单单位**的数量调整到合法精度 (向下取整)
    pub fn round_size_down(&self, size: f64) -> f64 {
        Self::round_to_step(size, self.size_step, RoundingStrategy::ToNegativeInfinity)
    }

    /// 将**币本位**数量调整到交易所允许的合法精度 (向下取整)。
    ///
    /// 合法的币数量必然是 `size_step × contract_size` 的整数倍，因此精度约束完全可以在币本位
    /// 里表达 —— 这让"精度取整"与"单位折算"得以分开：
    /// - 精度取整是**市场规则**，回测/模拟盘也必须遵守（否则会用非法 tick 价、非法步长成交，
    ///   撮合结果失真），故留在 domain 路径、用币本位表达
    /// - 单位折算是**线路细节**，只发生在交易所出口 (见 [`crate::exchange::ExchangeOrder`])
    ///
    /// 二者曾被合并在一个函数里，搬到出口后回测就悄悄丢掉了取整（同一天回测 PnL 出现偏差）。
    pub fn round_coin_size_down(&self, coin_amount: f64) -> f64 {
        self.qty_to_coin(self.round_size_down(self.coin_to_qty(coin_amount)))
    }

    /// 使用 Decimal 精确计算，按 step 取整
    ///
    /// value 和 step 正常来自交易所 API 的合法浮点数（非 NaN/Infinity）。若出现非有限值，
    /// 无法用 Decimal 精确取整，记录后返回原值兜底（不 panic）。
    fn round_to_step(value: f64, step: f64, strategy: RoundingStrategy) -> f64 {
        let (Some(value_dec), Some(step_dec)) = (Decimal::from_f64(value), Decimal::from_f64(step))
        else {
            tracing::warn!(value, step, "round_to_step: 非有限输入，返回原值不取整");
            return value;
        };

        let ticks = value_dec / step_dec;
        let rounded_ticks = ticks.round_dp_with_strategy(0, strategy);

        let result = rounded_ticks * step_dec;
        result.to_f64().unwrap_or(value)
    }
}
