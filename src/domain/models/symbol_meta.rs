use crate::domain::models::{Exchange, Symbol};
use crate::domain::PriceFormatter;
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
    /// 最小下单数量（交易所下单单位）
    pub min_order_size: f64,
    /// 交易所要求的最小下单金额（计价币，如 USDT）：币安 `MIN_NOTIONAL`、HL 10 USD；
    /// 只按数量限制的所（OKX 按一手、IBKR）为 0。
    ///
    /// 框架只如实提供；与价格有关，所以不在 [`Self::checked_exchange_qty`] 里校验 ——
    /// 用不用、怎么和自己的下限合并由策略决定。
    /// 模拟柜台与回测撮合也不按它拒单（回测里不会出现「低于金额下限被拒」）。
    pub min_notional: f64,
    /// 合约乘数 (OKX: cval, Binance: 1.0)
    ///
    /// 表示每张合约对应的币本位数量
    /// - OKX ETH: cval=0.1, 下单 qty=1 表示 0.1 ETH
    /// - Binance: 直接按币的数量下单, 等效于 cval=1.0
    pub contract_size: f64,
}

impl SymbolMeta {
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

    /// 币本位 -> 交易所下单单位：折算、按精度向下取整，并校验数量下界。
    ///
    /// **出向数量校验的唯一出处**：实盘出口（`crate::exchange::ExchangeOrder::from_domain`）
    /// 与模拟柜台（`PaperCounter`）都经此校验 —— 保证"实盘必拒的单，模拟盘也拒"，
    /// 否则模拟盘会成交实盘发不出去的单，仿真失真。
    ///
    /// - 取整后为 0（请求量不足一个 `size_step`）：照发交易所必拒，且策略会陷入
    ///   "下单 → 被拒 → pending 清除 → 下个事件再下"的静默重试环
    /// - 低于 `min_order_size`（交易所最小下单量，交易所单位）：同样必拒
    /// - 非有限值（NaN / 无穷）：上游算错了，拒绝而不是照发
    pub fn checked_exchange_qty(&self, coin_amount: f64) -> Result<f64, String> {
        // NaN 与任何数比较都为假，会穿过下面两道下界检查原样发出去
        if !coin_amount.is_finite() {
            return Err(format!("数量不是有限值：{coin_amount}"));
        }
        let Some((quantity, _)) = self.qty_floor_dec(coin_amount) else {
            return Err(format!(
                "品种规格无效或数量超范围：请求 {coin_amount}，step={}，contract_size={}",
                self.size_step, self.contract_size
            ));
        };
        let quantity = dec_to_f64(quantity);
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
    /// 规格无效（步长或乘数为 0）、数量非有限或超范围时返回 0，下游校验会拒。
    pub fn round_coin_size_down(&self, coin_amount: f64) -> f64 {
        self.qty_floor_dec(coin_amount).map_or(0.0, |(qty, cs)| dec_to_f64(qty * cs))
    }

    /// 币本位 → 交易所下单单位并按 `size_step` 向下取整，连同合约乘数一起以 Decimal 返回。
    /// **数量取整规则的唯一实现。**
    ///
    /// **折算、取整、折回都在 Decimal 里做**：浮点除法会把恰好在步长上的量算小一点
    /// （0.29 / 0.01 = 28.999999999999996），向下取整就少一步；策略、运行器、出口依次对同一个量
    /// 取整，每一次都可能再丢一步 —— 两所挂单量差一步、卡在最小下单量上的单被本地拒掉
    /// （11-13 框架审查）。精确运算下取整是幂等的：f64 转 Decimal 取最短十进制写法
    /// （0.29 → 0.29），Decimal 转回 f64 取最近值，取整过的量再取整不变。
    ///
    /// `None`：数量非有限或超出 Decimal 范围，或规格的步长 / 乘数为 0。
    fn qty_floor_dec(&self, coin_amount: f64) -> Option<(Decimal, Decimal)> {
        let coin = Decimal::from_f64(coin_amount)?;
        let cs = Decimal::from_f64(self.contract_size).filter(|d| !d.is_zero())?;
        let step = Decimal::from_f64(self.size_step).filter(|d| !d.is_zero())?;
        // 几所浮点持仓相加的差会落在整步稍下方（0.7 - 0.64 → 0.0599999999999999，按 0.01 取整只剩
        // 5 步）：补腿少下一步，剩下那一步又被当零头永远不补（#764 审查）。按步数留 1e-9 的余量，
        // 浮点噪声（约 1e-14 步）被吸收，真实的零头（远大于 1e-9 步）照常舍掉；取整仍幂等。
        let steps = (coin / cs / step + STEP_EPSILON).floor();
        Some((steps * step, cs))
    }
}

/// 向下取整时按步数留的余量，见 `qty_floor_dec`
const STEP_EPSILON: Decimal = Decimal::from_parts(1, 0, 0, false, 9);

/// Decimal → f64 总能成功（取最近值）
fn dec_to_f64(d: Decimal) -> f64 {
    d.to_f64().expect("Decimal 转 f64 不会失败")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::StepFormatter;

    fn meta(contract_size: f64, size_step: f64) -> SymbolMeta {
        SymbolMeta {
            exchange: Exchange::OKX,
            symbol: "X".into(),
            price_formatter: Arc::new(StepFormatter::new(0.01)),
            size_step,
            min_order_size: size_step,
            min_notional: 0.0,
            contract_size,
        }
    }

    /// 恰好在步长上的量取整不丢步，取整过的量再取整不变（幂等）；出口换算的张数就是那个步数。
    /// 旧实现（浮点除法）在 contract_size=0.01、k=29 等处少一步。
    #[test]
    fn rounding_is_exact_and_idempotent() {
        for (cs, step) in [(0.01, 1.0), (0.1, 1.0), (0.001, 1.0), (1.0, 0.001), (10.0, 1.0), (100.0, 1.0), (0.1, 0.1), (1.0, 0.1), (0.0001, 1.0)] {
            let m = meta(cs, step);
            let exact = Decimal::from_f64(step).unwrap() * Decimal::from_f64(cs).unwrap();
            for k in 1..20_000u32 {
                // k 步对应的币量（最近的 f64），即上游第一次取整后交下来的量
                let coin = (Decimal::from(k) * exact).to_f64().unwrap();
                assert_eq!(m.round_coin_size_down(coin), coin, "cs={cs} step={step} k={k}");
                let units = m.checked_exchange_qty(coin).unwrap();
                assert_eq!(
                    Decimal::from_f64(units).unwrap(),
                    Decimal::from(k) * Decimal::from_f64(step).unwrap(),
                    "cs={cs} step={step} k={k} coin={coin}"
                );
                assert_eq!(m.round_coin_size_down(coin), coin, "cs={cs} step={step} k={k}");
            }
        }
    }

    #[test]
    fn rounds_down_between_steps() {
        let m = meta(0.01, 1.0);
        assert_eq!(m.round_coin_size_down(0.299), 0.29);
        assert!(m.checked_exchange_qty(0.009).is_err());
        assert!(m.checked_exchange_qty(f64::NAN).is_err());
    }

    /// 浮点相减落在整步稍下方的量不丢一步（补腿量来自几所持仓相加，#764 审查）
    #[test]
    fn float_noise_below_a_step_is_not_lost() {
        let m = meta(1.0, 0.01);
        assert_eq!(m.round_coin_size_down(0.7 - 0.64), 0.06);
        assert_eq!(m.round_coin_size_down(-(0.7 - 0.64)), -0.06);
        assert_eq!(m.round_coin_size_down(0.0599), 0.05);
    }
}