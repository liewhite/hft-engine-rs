use crate::domain::models::{Exchange, Symbol};
use crate::domain::{PositiveDecimal, Price, PriceRule, Quantity};
use rust_decimal::Decimal;
use std::sync::Arc;

/// 交易对元数据
///
/// 步长、最小下单量、合约乘数都是精确十进制（交易所规格本来就是十进制字符串）：
/// 按步长取整、单位折算都是精确运算，没有浮点余量。三者在类型上就严格为正
/// （[`PositiveDecimal`]），不合法的规格在各所加载处就被剔除，取整与折算不再判 0。
#[derive(Debug, Clone)]
pub struct SymbolMeta {
    pub exchange: Exchange,
    pub symbol: Symbol,
    /// 价格精度规则
    pub price_rule: Arc<dyn PriceRule>,
    /// 数量精度 (最小数量变动单位，交易所下单单位)
    pub size_step: PositiveDecimal,
    /// 最小下单数量（交易所下单单位）
    pub min_order_size: PositiveDecimal,
    /// 交易所要求的最小下单金额（计价币，如 USDT）：币安 `MIN_NOTIONAL`、HL 10 USD；
    /// 只按数量限制的所（OKX 按一手）为 0。
    ///
    /// 框架只如实提供；与价格有关，所以不在 [`Self::checked_exchange_qty`] 里校验 ——
    /// 用不用、怎么和自己的下限合并由策略决定。
    /// 模拟柜台与回测撮合也不按它拒单（回测里不会出现「低于金额下限被拒」）。
    pub min_notional: f64,
    /// 合约乘数 (OKX: cval, Binance: 1)
    ///
    /// 表示每张合约对应的币本位数量
    /// - OKX ETH: cval=0.1, 下单 qty=1 表示 0.1 ETH
    /// - Binance: 直接按币的数量下单, 等效于 cval=1
    pub contract_size: PositiveDecimal,
    /// 计价币：价格、成交金额与持仓价值的币种（交易所原样的名字，如 USDT、USDC）。
    /// 框架如实标出、不换算（#841）；不同计价币的金额要不要合并由策略决定。
    pub quote: String,
}

impl SymbolMeta {
    /// 将下单数量转换为币本位数量
    ///
    /// 例如: 下单 5 张, OKX cval=0.1, 则返回 0.5 ETH
    pub fn qty_to_coin(&self, qty: Decimal) -> Quantity {
        qty * self.contract_size.get()
    }

    /// 价格取到合法精度后写成下单字符串（无尾零、无科学计数法）
    pub fn format_price(&self, price: Price) -> String {
        crate::domain::to_wire(self.round_price(price))
    }

    /// 将价格取到最近的合法精度
    pub fn round_price(&self, price: Price) -> Price {
        self.price_rule.round(price)
    }

    /// 币本位 -> 交易所下单单位：折算、按精度向 0 取整，并校验数量下界。
    ///
    /// **出向数量校验的唯一出处**：实盘出口（`crate::exchange::ExchangeOrder::from_domain`）
    /// 与模拟柜台（`PaperCounter`）都经此校验 —— 保证"实盘必拒的单，模拟盘也拒"，
    /// 否则模拟盘会成交实盘发不出去的单，仿真失真。
    ///
    /// - 取整后为 0（请求量不足一个 `size_step`）：照发交易所必拒，且策略会陷入
    ///   "下单 → 被拒 → pending 清除 → 下个事件再下"的静默重试环
    /// - 低于 `min_order_size`（交易所最小下单量，交易所单位）：同样必拒
    /// - 负数：下单数量只能为正，方向在 `Side` 里，负数说明上游算错了
    pub fn checked_exchange_qty(&self, coin_amount: Quantity) -> Result<Decimal, String> {
        if coin_amount.is_sign_negative() {
            return Err(format!("数量为负：{coin_amount}"));
        }
        let quantity = self.units_trunc(coin_amount);
        if quantity.is_zero() {
            return Err(format!(
                "数量取整后为 0：请求 {coin_amount} 币不足一个 size_step（step={}, contract_size={}）",
                self.size_step, self.contract_size
            ));
        }
        if quantity < self.min_order_size.get() {
            return Err(format!(
                "数量低于交易所最小下单量：取整后 {quantity}，最小 {}（交易所单位）",
                self.min_order_size
            ));
        }
        Ok(quantity)
    }

    /// 将**币本位**数量调整到交易所允许的合法精度（向 0 取整，宁少勿多）。
    ///
    /// 合法的币数量必然是 `size_step × contract_size` 的整数倍，因此精度约束完全可以在币本位
    /// 里表达 —— 这让"精度取整"与"单位折算"得以分开：
    /// - 精度取整是**市场规则**，回测/模拟盘也必须遵守（否则会用非法 tick 价、非法步长成交，
    ///   撮合结果失真），故留在 domain 路径、用币本位表达
    /// - 单位折算是**线路细节**，只发生在交易所出口 (见 [`crate::exchange::ExchangeOrder`])
    ///
    /// 精确运算，取整幂等。
    pub fn round_coin_size_down(&self, coin_amount: Quantity) -> Quantity {
        self.units_trunc(coin_amount) * self.contract_size.get()
    }

    /// 币本位 → 交易所下单单位并按 `size_step` 向 0 取整。**数量取整规则的唯一实现。**
    fn units_trunc(&self, coin_amount: Quantity) -> Decimal {
        let step = self.size_step.get();
        (coin_amount / self.contract_size.get() / step).trunc() * step
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::PriceStep;
    use rust_decimal_macros::dec;

    fn meta(contract_size: Decimal, size_step: Decimal) -> SymbolMeta {
        let size_step = PositiveDecimal::new(size_step).unwrap();
        SymbolMeta {
            exchange: Exchange::OKX,
            symbol: "X".into(),
            price_rule: Arc::new(PriceStep::new(
                crate::domain::PositiveDecimal::new(dec!(0.01)).unwrap(),
            )),
            size_step,
            min_order_size: size_step,
            min_notional: 0.0,
            contract_size: PositiveDecimal::new(contract_size).unwrap(),
            quote: "USDT".to_string(),
        }
    }

    /// 恰好在步长上的量取整不丢步，取整过的量再取整不变（幂等）；出口换算的张数就是那个步数
    #[test]
    fn rounding_is_exact_and_idempotent() {
        for (cs, step) in [
            (dec!(0.01), dec!(1)),
            (dec!(0.1), dec!(1)),
            (dec!(0.001), dec!(1)),
            (dec!(1), dec!(0.001)),
            (dec!(10), dec!(1)),
            (dec!(100), dec!(1)),
            (dec!(0.1), dec!(0.1)),
            (dec!(1), dec!(0.1)),
            (dec!(0.0001), dec!(1)),
        ] {
            let m = meta(cs, step);
            for k in 1..20_000u32 {
                let coin = Decimal::from(k) * step * cs;
                assert_eq!(
                    m.round_coin_size_down(coin),
                    coin,
                    "cs={cs} step={step} k={k}"
                );
                assert_eq!(
                    m.checked_exchange_qty(coin).unwrap(),
                    Decimal::from(k) * step,
                    "cs={cs} step={step} k={k}"
                );
            }
        }
    }

    #[test]
    fn rounds_down_between_steps() {
        let m = meta(dec!(0.01), dec!(1));
        assert_eq!(m.round_coin_size_down(dec!(0.299)), dec!(0.29));
        assert!(m.checked_exchange_qty(dec!(0.009)).is_err(), "不足一手");
        assert!(m.checked_exchange_qty(dec!(-0.29)).is_err(), "负数");
        let m = meta(dec!(1), dec!(0.01));
        assert_eq!(m.round_coin_size_down(dec!(0.0599)), dec!(0.05));
        assert_eq!(
            m.round_coin_size_down(dec!(-0.0599)),
            dec!(-0.05),
            "向 0 取整"
        );
    }

    /// 几所持仓相加相减的差恰好在整步上，取整不丢步（f64 时 0.7 - 0.64 = 0.0599999…，曾少补一步，#764）
    #[test]
    fn position_differences_stay_on_step() {
        let m = meta(dec!(1), dec!(0.01));
        assert_eq!(m.round_coin_size_down(dec!(0.7) - dec!(0.64)), dec!(0.06));
        assert_eq!(
            m.checked_exchange_qty(dec!(0.7) - dec!(0.64)).unwrap(),
            dec!(0.06)
        );
    }

    #[test]
    fn below_min_order_size_is_rejected() {
        let mut m = meta(dec!(1), dec!(0.001));
        m.min_order_size = PositiveDecimal::new(dec!(0.01)).unwrap();
        assert!(m.checked_exchange_qty(dec!(0.009)).is_err());
        assert_eq!(m.checked_exchange_qty(dec!(0.0109)).unwrap(), dec!(0.010));
    }

    #[test]
    fn price_is_written_without_trailing_zeros() {
        let mut m = meta(dec!(1), dec!(1));
        m.price_rule = Arc::new(PriceStep::new(
            crate::domain::PositiveDecimal::new(dec!(0.10)).unwrap(),
        ));
        assert_eq!(m.format_price(dec!(100.04)), "100");
        assert_eq!(m.format_price(dec!(4870)), "4870");
        assert_eq!(m.format_price(dec!(0.25)), "0.3");
    }
}
