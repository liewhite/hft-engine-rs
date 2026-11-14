use crate::domain::models::{Fill, FillReason, OrderStatus, OrderUpdate, Side};
use std::collections::BTreeMap;

/// 成交与订单的累计统计（一条腿：一个币在一个所，金额都以该合约的计价币计）
///
/// 纯数据 + 纯函数：只由事件驱动累加，不读时钟、不做 IO，可直接单测。
///
/// 价差盈亏口径为**现金流 + 存货估值**：
/// - `cash`：卖出收现金、买入付现金（计价币，不含手续费）
/// - 价差盈亏 = `cash` + 本次会话的存货估值变化（由调用方按持仓读数算）
///
/// 手续费按交易所实际扣的币种另记在 `fees`，不并进 `cash`：币种可能不是计价币（BNB 抵扣、
/// 现货买卖两边不同币），不同币不能相加（#817）。
///
/// 这样不需要把开仓与平仓配对（跨所、部分成交、强平会让配对逻辑迅速失控），
/// 对任意成交顺序都成立。
#[derive(Debug, Clone, Default)]
pub struct TradingStats {
    /// 成交笔数
    pub fills: u64,
    /// 累计成交名义额 (price * size，计价币)
    pub volume: f64,
    /// 按币种累计的手续费（正数 = 支出，负数 = 返佣）；只含出现过的币种
    pub fees: BTreeMap<String, f64>,
    /// 累计现金流（计价币，不含手续费）：卖出 +notional，买入 -notional
    pub cash: f64,
    /// 强平 / ADL 成交笔数（风险告警指标，正常应恒为 0）
    pub forced_fills: u64,
    /// 被拒绝或出错的订单数
    pub rejected_orders: u64,
}

impl TradingStats {
    /// 累加一笔成交
    pub fn apply_fill(&mut self, fill: &Fill) {
        let notional = crate::domain::to_f64(fill.price * fill.size);
        self.fills += 1;
        self.volume += notional;
        if let Some(fee) = &fill.fee {
            *self.fees.entry(fee.asset.clone()).or_default() += fee.amount;
        }
        self.cash += match fill.side {
            Side::Long => -notional,
            Side::Short => notional,
        };
        if fill.reason != FillReason::Normal {
            self.forced_fills += 1;
        }
    }

    /// 累加一条交易所订单回报（只统计拒单）
    pub fn apply_order_update(&mut self, update: &OrderUpdate) {
        if matches!(update.status, OrderStatus::Rejected { .. }) {
            self.rejected_orders += 1;
        }
    }

    /// 累加一次本地下单请求被拒（REST 拒单、出口校验不过；口径与交易所拒单合在一个计数里）
    pub fn apply_place_rejected(&mut self) {
        self.rejected_orders += 1;
    }

    /// 价差盈亏 = 累计现金流 + 存货估值变化（不含手续费，手续费按币种见 `fees`）
    pub fn total_pnl(&self, inventory_value: f64) -> f64 {
        self.cash + inventory_value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Exchange, Money};
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    fn fill(side: Side, price: Decimal, size: Decimal, fee: f64, reason: FillReason) -> Fill {
        fill_with_fee(
            side,
            price,
            size,
            crate::domain::fee_from_report(fee, Some("USDT")).unwrap(),
            reason,
        )
    }

    fn fill_with_fee(
        side: Side,
        price: Decimal,
        size: Decimal,
        fee: Option<Money>,
        reason: FillReason,
    ) -> Fill {
        Fill {
            exchange: Exchange::Binance,
            symbol: "BTC".to_string(),
            side,
            price,
            size,
            client_order_id: None,
            order_id: "1".to_string(),
            timestamp: 0,
            fee,
            reason,
        }
    }

    #[test]
    fn round_trip_pnl_is_spread_and_fees_are_kept_apart() {
        let mut stats = TradingStats::default();
        stats.apply_fill(&fill(
            Side::Long,
            dec!(100.0),
            dec!(1.0),
            0.1,
            FillReason::Normal,
        ));
        stats.apply_fill(&fill(
            Side::Short,
            dec!(101.0),
            dec!(1.0),
            0.1,
            FillReason::Normal,
        ));

        // 买 100 卖 101 → 价差 1（不扣手续费）；双边各 0.1 USDT 手续费另记
        assert!((stats.total_pnl(0.0) - 1.0).abs() < 1e-9);
        assert_eq!(stats.fills, 2);
        assert!((stats.volume - 201.0).abs() < 1e-9);
        assert_eq!(stats.fees.len(), 1);
        assert!((stats.fees["USDT"] - 0.2).abs() < 1e-9);
    }

    #[test]
    fn fees_in_different_assets_are_not_added_together() {
        let mut stats = TradingStats::default();
        stats.apply_fill(&fill_with_fee(
            Side::Long,
            dec!(100.0),
            dec!(1.0),
            Some(Money::new(0.0003, "BNB")),
            FillReason::Normal,
        ));
        stats.apply_fill(&fill_with_fee(
            Side::Short,
            dec!(100.0),
            dec!(1.0),
            Some(Money::new(0.05, "USDC")),
            FillReason::Normal,
        ));
        assert_eq!(stats.fees.len(), 2);
        assert!((stats.fees["BNB"] - 0.0003).abs() < 1e-12);
        assert!((stats.fees["USDC"] - 0.05).abs() < 1e-12);
        assert!(stats.cash.abs() < 1e-9, "现金流不混入手续费");
    }

    #[test]
    fn open_position_pnl_uses_inventory_value() {
        let mut stats = TradingStats::default();
        stats.apply_fill(&fill(
            Side::Long,
            dec!(100.0),
            dec!(2.0),
            0.0,
            FillReason::Normal,
        ));

        // 持有 2 个、现价 105 → 市值 210，现金流 -200 → 浮盈 10
        assert!((stats.total_pnl(210.0) - 10.0).abs() < 1e-9);
    }

    #[test]
    fn counts_forced_fills_separately() {
        let mut stats = TradingStats::default();
        stats.apply_fill(&fill(
            Side::Short,
            dec!(100.0),
            dec!(1.0),
            0.0,
            FillReason::Liquidation,
        ));
        stats.apply_fill(&fill(
            Side::Short,
            dec!(100.0),
            dec!(1.0),
            0.0,
            FillReason::Normal,
        ));
        assert_eq!(stats.forced_fills, 1);
        assert_eq!(stats.fills, 2);
    }

    #[test]
    fn counts_rejected_orders() {
        let mut stats = TradingStats::default();
        let mut update = OrderUpdate {
            order_id: "1".to_string(),
            client_order_id: None,
            exchange: Exchange::Binance,
            symbol: "BTC".to_string(),
            side: Side::Long,
            status: OrderStatus::Rejected {
                reason: "insufficient margin".to_string(),
            },
            quantity: dec!(0.0),
            filled_quantity: dec!(0.0),
        };
        stats.apply_order_update(&update);
        update.status = OrderStatus::Filled;
        stats.apply_order_update(&update);
        assert_eq!(stats.rejected_orders, 1);
    }
}
