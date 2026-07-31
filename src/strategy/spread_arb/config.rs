use serde::Deserialize;

/// 跨所价差套利策略配置
#[derive(Debug, Clone, Deserialize)]
pub struct SpreadArbConfig {
    /// EMA 周期（表示最近多少笔 BBO 更新的均价）
    pub ema_period: usize,
    /// 开仓 deviation 阈值
    ///
    /// - max_bid_deviation + max_ask_deviation >= deviation_threshold 时开仓
    /// - **该阈值已隐含全部交易成本**：策略不单独建模手续费，双边 taker 费率、
    ///   `ioc_slippage` 让价、以及期望净收益都必须在调参时一并折进这个数值。
    ///   即 deviation_threshold 是"净收益门槛"，不是"毛价差门槛"。
    pub deviation_threshold: f64,
    /// 单笔下单金额 (USDT)，开平仓均按此金额计算数量
    pub max_notional: f64,
    /// 最小下单金额 (USDT)，低于此金额的订单将被放弃
    pub min_notional: f64,
    /// 订单超时时间 (毫秒)
    pub order_timeout_ms: u64,
    /// 单 symbol 单交易所持仓名义价值上限 (USDT)
    ///
    /// - |新仓位| * mid_price 超过此值、且该单为增仓方向时，该侧不下单
    /// - 与 max_symbol_leverage 互补：本项是绝对值上限，后者是相对 equity 的比例上限
    /// - 不拦截减仓
    pub max_position_notional: f64,
    /// 单边仓位占账户 equity 的最大比例
    /// - 任一交易所的仓位价值 / equity 超过此比例时禁止增仓
    /// - 不拦截减仓
    pub max_symbol_leverage: f64,
    /// 账户级别最大杠杆率 (account_notional / equity)
    /// - 任一交易所超过此阈值，且订单方向与现有仓位方向相同时，禁止开仓
    /// - 用于控制账户整体风险
    pub max_account_leverage: f64,
    /// IOC 订单滑点（用限价单 IOC 模拟市价单）
    /// - 例如 0.001 表示 0.1%
    /// - 做多时 ask + slippage，做空时 bid - slippage
    pub ioc_slippage: f64,
    /// BBO 最大可用年龄 (毫秒)
    ///
    /// 产生信号时校验参与交易的两个交易所的 BBO **本地接收时刻**距今不超过此值，
    /// 否则视为行情陈旧、本轮不下单。防止某所 WS 静默停推（TCP 未断、无数据）时
    /// 策略在冻结的价格上反复吃单。
    pub max_bbo_age_ms: u64,
}

impl SpreadArbConfig {
    /// 配置 sanity 检查
    ///
    /// 在启动期一次性校验，失败即拒绝启动——不做运行期静默降级（0 值风控等于没有风控）。
    pub fn validate(&self) -> Result<(), String> {
        if self.ema_period == 0 {
            return Err("ema_period 必须 > 0".to_string());
        }
        if !(self.deviation_threshold.is_finite() && self.deviation_threshold > 0.0) {
            return Err("deviation_threshold 必须为正有限值（需覆盖双边手续费与滑点）".to_string());
        }
        if !(self.max_notional.is_finite() && self.max_notional > 0.0) {
            return Err("max_notional 必须 > 0".to_string());
        }
        if !(self.min_notional.is_finite() && self.min_notional > 0.0) {
            return Err("min_notional 必须 > 0".to_string());
        }
        if self.min_notional > self.max_notional {
            return Err("min_notional 不能大于 max_notional".to_string());
        }
        if self.order_timeout_ms == 0 {
            return Err("order_timeout_ms 必须 > 0（否则超时订单永不清理）".to_string());
        }
        if !(self.max_position_notional.is_finite() && self.max_position_notional > 0.0) {
            return Err("max_position_notional 必须 > 0".to_string());
        }
        if self.max_position_notional < self.max_notional {
            return Err("max_position_notional 不应小于 max_notional（否则首笔即被拦截）".to_string());
        }
        if !(self.max_symbol_leverage.is_finite() && self.max_symbol_leverage > 0.0) {
            return Err("max_symbol_leverage 必须 > 0".to_string());
        }
        if !(self.max_account_leverage.is_finite() && self.max_account_leverage > 0.0) {
            return Err("max_account_leverage 必须 > 0".to_string());
        }
        if !(self.ioc_slippage.is_finite() && self.ioc_slippage >= 0.0) {
            return Err("ioc_slippage 必须为非负有限值".to_string());
        }
        if self.ioc_slippage >= self.deviation_threshold {
            return Err(format!(
                "ioc_slippage ({}) 不应 >= deviation_threshold ({})：让价幅度吃掉全部预期收益",
                self.ioc_slippage, self.deviation_threshold
            ));
        }
        if self.max_bbo_age_ms == 0 {
            return Err("max_bbo_age_ms 必须 > 0（否则任何行情都被判为陈旧）".to_string());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> SpreadArbConfig {
        SpreadArbConfig {
            ema_period: 100,
            deviation_threshold: 0.004,
            max_notional: 1000.0,
            min_notional: 10.0,
            order_timeout_ms: 10_000,
            max_position_notional: 5000.0,
            max_symbol_leverage: 0.5,
            max_account_leverage: 3.0,
            ioc_slippage: 0.001,
            max_bbo_age_ms: 3000,
        }
    }

    #[test]
    fn accepts_valid_config() {
        assert!(valid().validate().is_ok());
    }

    #[test]
    fn rejects_slippage_eating_the_whole_edge() {
        let cfg = SpreadArbConfig {
            deviation_threshold: 0.0005,
            ioc_slippage: 0.001,
            ..valid()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_zero_risk_limits() {
        for cfg in [
            SpreadArbConfig {
                max_symbol_leverage: 0.0,
                ..valid()
            },
            SpreadArbConfig {
                max_account_leverage: 0.0,
                ..valid()
            },
            SpreadArbConfig {
                max_position_notional: 0.0,
                ..valid()
            },
            SpreadArbConfig {
                max_bbo_age_ms: 0,
                ..valid()
            },
            SpreadArbConfig {
                order_timeout_ms: 0,
                ..valid()
            },
            SpreadArbConfig {
                ema_period: 0,
                ..valid()
            },
        ] {
            assert!(cfg.validate().is_err(), "应拒绝: {cfg:?}");
        }
    }

    #[test]
    fn rejects_position_limit_below_single_order() {
        let cfg = SpreadArbConfig {
            max_notional: 1000.0,
            max_position_notional: 500.0,
            ..valid()
        };
        assert!(cfg.validate().is_err());
    }
}
