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

    /// **币本位**的最小可交易增量 = `size_step × contract_size`。
    ///
    /// `size_step` 是**交易所下单单位**（OKX 的 SWAP 是张）的步长，不能直接当币量用；
    /// 合法的币数量是本值的整数倍（见 [`Self::round_coin_size_down`]）。凡是拿"最小增量"
    /// 与币本位数量（持仓、Fill）比较的地方，都必须用本值。
    pub fn coin_size_step(&self) -> f64 {
        self.size_step * self.contract_size
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

    /// 币本位 -> 交易所下单单位：折算、按精度向下取整，并校验数量下界。
    ///
    /// **出向数量校验的唯一出处**：实盘出口（`crate::exchange::ExchangeOrder::from_domain`）
    /// 与模拟柜台（`PaperCounterActor`）都经此校验 —— 保证"实盘必拒的单，模拟盘也拒"，
    /// 否则模拟盘会成交实盘发不出去的单，仿真失真。
    ///
    /// - 取整后为 0（请求量不足一个 `size_step`）：照发交易所必拒，且策略会陷入
    ///   "下单 → 被拒 → pending 清除 → 下个事件再下"的静默重试环
    /// - 低于 `min_order_size`（交易所最小下单量，交易所单位）：同样必拒
    pub fn checked_exchange_qty(&self, coin_amount: f64) -> Result<f64, String> {
        let quantity = self.round_size_down(self.coin_to_qty(coin_amount));
        if quantity <= 0.0 {
            return Err(format!(
                "数量取整后为 0：请求 {coin_amount} 币不足一个 size_step（step={}, contract_size={}）",
                self.size_step, self.contract_size
            ));
        }
        if quantity < self.min_order_size {
            return Err(format!(
                "数量低于交易所最小下单量：取整后 {quantity}，最小 {}（交易所单位）",
                self.min_order_size
            ));
        }
        Ok(quantity)
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
