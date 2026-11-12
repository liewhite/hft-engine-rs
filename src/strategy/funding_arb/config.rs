use crate::domain::Symbol;
use serde::Deserialize;

/// 资金费套利（多所：币安 / Hyperliquid / OKX）配置。
///
/// 杠杆按**交易所各自**算（两所资金不互通，风险聚集在单边）：
/// - 单币：该币在该所的持仓价值 / 该所账户净值 ≤ `max_symbol_leverage`
/// - 全局：该所全部持仓价值 / 该所账户净值 ≤ `max_account_leverage`
///
/// 超限只停止开仓，不拦平仓与补腿。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FundingArbConfig {
    /// 开仓门槛：两所「过去 24h 实际资金费累加 × 365」之差的绝对值，0.5 = 50%
    pub open_threshold_apr: f64,
    /// 平仓门槛：一对相反仓位的收益年化（空头所年化 − 多头所年化）低于此值即在这两所之间平仓，
    /// 0.1 = 10%；须 > 0 且小于开仓门槛
    pub close_threshold_apr: f64,
    /// 单笔挂单金额上限（USD）；实际取 min(本值, 两所对手盘第一档的金额)
    pub order_notional: f64,
    /// 单笔最小金额（USD）的配置下限。每所实际用它与该所交易所下限（币安 MIN_NOTIONAL、
    /// HL 10 USD）中较大的一个：低于它不挂单；持仓差的价值低于补腿所的下限也不补腿（补了会被拒）
    pub min_order_notional: f64,
    /// 账户级杠杆上限（每所各自）
    pub max_account_leverage: f64,
    /// 默认单币杠杆上限（每所各自），pair 可单独覆盖
    pub max_symbol_leverage: f64,
    /// 默认挂单距离：卖单挂在 卖一 × (1 + d)，买单挂在 买一 × (1 - d)；pair 可单独覆盖
    pub order_distance: f64,
    /// 挂单价偏离目标价超过 `order_distance × requote_ratio` 才撤单重挂。
    /// 太小会频繁撤挂：HL 按地址累计成交额限制下单次数，撤挂过多会被限流。
    pub requote_ratio: f64,
    /// 补腿下单（市价）失败后，至少隔多久再试；撤单未生效时也按此间隔重撤（毫秒）
    pub hedge_retry_ms: u64,
    /// 成交回报结算窗口（毫秒）：任一所有本策略的单离开挂单列表后，等这么久再按持仓差补腿。
    /// 订单终态与成交回报是交易所两条无序推送，终态先到时持仓还没加上，立刻补会多补一份。
    pub fill_settle_ms: u64,
    /// 挂单至少挂这么久才因价格偏离撤换（毫秒）。HL 下单次数按成交额回补，频繁撤挂会耗尽额度
    pub min_requote_ms: u64,
    /// 盘口最大可用年龄（毫秒，按本地接收时刻）；任一所超龄不开新单
    pub max_bbo_age_ms: u64,
    /// 资金费数据最大可用年龄（毫秒）；超龄视为未知，不开不平
    pub max_funding_age_ms: u64,
    /// 要做的币
    pub pairs: Vec<PairConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairConfig {
    /// 内部符号（各所同名，如 "BTC"、"TSLA"）。在哪几所做由启动时各所品种目录决定（至少两所）
    pub symbol: Symbol,
    #[serde(default)]
    pub order_distance: Option<f64>,
    #[serde(default)]
    pub max_symbol_leverage: Option<f64>,
}

/// 单个币的生效参数（全局默认 + pair 覆盖，在一处合并）
#[derive(Debug, Clone)]
pub struct PairParams {
    pub symbol: Symbol,
    pub open_threshold_apr: f64,
    pub close_threshold_apr: f64,
    pub order_notional: f64,
    pub min_order_notional: f64,
    pub max_account_leverage: f64,
    pub max_symbol_leverage: f64,
    pub order_distance: f64,
    pub requote_ratio: f64,
    pub hedge_retry_ms: u64,
    pub fill_settle_ms: u64,
    pub min_requote_ms: u64,
    pub max_bbo_age_ms: u64,
    pub max_funding_age_ms: u64,
}

impl FundingArbConfig {
    /// 启动期一次校验，失败拒绝启动（0 值风控等于没有风控）。
    pub fn validate(&self) -> Result<(), String> {
        let pos = |name: &str, v: f64| {
            if v.is_finite() && v > 0.0 {
                Ok(())
            } else {
                Err(format!("{name} 必须为正有限值，当前 {v}"))
            }
        };
        pos("open_threshold_apr", self.open_threshold_apr)?;
        // > 0：仓位对收益 ≤ 0 时一定平（信号文档「开仓只会削到零头」的论证依赖它）
        if !(self.close_threshold_apr.is_finite()
            && self.close_threshold_apr > 0.0
            && self.close_threshold_apr < self.open_threshold_apr)
        {
            return Err(format!(
                "close_threshold_apr 必须 > 0 且小于 open_threshold_apr（当前 {} / {}）",
                self.close_threshold_apr, self.open_threshold_apr
            ));
        }
        pos("order_notional", self.order_notional)?;
        pos("min_order_notional", self.min_order_notional)?;
        pos("max_account_leverage", self.max_account_leverage)?;
        pos("requote_ratio", self.requote_ratio)?;
        if self.min_order_notional > self.order_notional {
            return Err("min_order_notional 不能大于 order_notional".into());
        }
        if self.max_bbo_age_ms == 0 || self.max_funding_age_ms == 0 || self.hedge_retry_ms == 0 || self.fill_settle_ms == 0 {
            return Err("max_bbo_age_ms / max_funding_age_ms / hedge_retry_ms / fill_settle_ms 必须 > 0".into());
        }
        if self.pairs.is_empty() {
            return Err("pairs 不能为空".into());
        }
        let mut seen = std::collections::HashSet::new();
        for p in self.params() {
            if !seen.insert(p.symbol.clone()) {
                return Err(format!("pairs 里 {} 重复", p.symbol));
            }
            pos(&format!("{}.order_distance", p.symbol), p.order_distance)?;
            pos(&format!("{}.max_symbol_leverage", p.symbol), p.max_symbol_leverage)?;
            if p.max_symbol_leverage > p.max_account_leverage {
                return Err(format!("{} 单币杠杆上限大于账户杠杆上限", p.symbol));
            }
        }
        Ok(())
    }

    /// 各币生效参数
    pub fn params(&self) -> Vec<PairParams> {
        self.pairs
            .iter()
            .map(|p| PairParams {
                symbol: p.symbol.clone(),
                open_threshold_apr: self.open_threshold_apr,
                close_threshold_apr: self.close_threshold_apr,
                order_notional: self.order_notional,
                min_order_notional: self.min_order_notional,
                max_account_leverage: self.max_account_leverage,
                max_symbol_leverage: p.max_symbol_leverage.unwrap_or(self.max_symbol_leverage),
                order_distance: p.order_distance.unwrap_or(self.order_distance),
                requote_ratio: self.requote_ratio,
                hedge_retry_ms: self.hedge_retry_ms,
                fill_settle_ms: self.fill_settle_ms,
                min_requote_ms: self.min_requote_ms,
                max_bbo_age_ms: self.max_bbo_age_ms,
                max_funding_age_ms: self.max_funding_age_ms,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> FundingArbConfig {
        serde_json::from_value(serde_json::json!({
            "open_threshold_apr": 0.5, "close_threshold_apr": 0.1, "order_notional": 200.0, "min_order_notional": 11.0,
            "max_account_leverage": 1.5, "max_symbol_leverage": 0.2, "order_distance": 0.0005,
            "requote_ratio": 1.0, "hedge_retry_ms": 2000, "fill_settle_ms": 1500, "min_requote_ms": 5000, "max_bbo_age_ms": 5000,
            "max_funding_age_ms": 7200000,
            "pairs": [{"symbol": "BTC"}, {"symbol": "XMR", "order_distance": 0.001, "max_symbol_leverage": 0.1}]
        }))
        .unwrap()
    }

    #[test]
    fn pair_overrides_win_over_defaults() {
        let c = cfg();
        c.validate().unwrap();
        let p = c.params();
        assert_eq!(p[0].order_distance, 0.0005);
        assert_eq!(p[1].order_distance, 0.001);
        assert_eq!(p[1].max_symbol_leverage, 0.1);
    }

    #[test]
    fn rejects_duplicates_and_zero_limits() {
        let mut c = cfg();
        c.pairs.push(PairConfig { symbol: "BTC".into(), order_distance: None, max_symbol_leverage: None });
        assert!(c.validate().is_err());
        let mut c = cfg();
        c.max_account_leverage = 0.0;
        assert!(c.validate().is_err());
        let mut c = cfg();
        c.close_threshold_apr = 0.5;
        assert!(c.validate().is_err(), "平仓门槛不小于开仓门槛会开了就平");
    }
}
