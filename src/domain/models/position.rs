use crate::domain::models::{Exchange, Side, Symbol};
use crate::domain::types::Quantity;

/// 仓位信息
///
/// size 为正表示多头，为负表示空头
#[derive(Debug, Clone, PartialEq)]
pub struct Position {
    pub exchange: Exchange,
    pub symbol: Symbol,
    /// 仓位数量：正数为多头，负数为空头
    pub size: Quantity,
    pub entry_price: f64,
    pub unrealized_pnl: f64,
}

impl Position {
    /// 仓位比较的浮点精度阈值
    pub const EPSILON: f64 = 1e-10;

    /// 判断是否空仓 (使用 epsilon 比较避免浮点精度问题)
    pub fn is_empty(&self) -> bool {
        self.size.abs() < Self::EPSILON
    }

    /// 获取持仓方向 (根据 size 符号)
    ///
    /// 返回 `None` 表示空仓
    pub fn side(&self) -> Option<Side> {
        if self.is_empty() {
            None
        } else if self.size > 0.0 {
            Some(Side::Long)
        } else {
            Some(Side::Short)
        }
    }

    /// 创建空仓位
    pub fn empty(exchange: Exchange, symbol: Symbol) -> Self {
        Self {
            exchange,
            symbol,
            size: 0.0,
            entry_price: 0.0,
            unrealized_pnl: 0.0,
        }
    }

    /// 由交易所读数构造持仓：**持仓非零时，价格类字段必须有值**。
    ///
    /// # 为什么这条规则值得一个独立出处
    ///
    /// 交易所在空仓时普遍不给价格字段（OKX 给空串、Hyperliquid 给 `null`、IBKR 干脆
    /// 没这个键），所以"没有 avgPx"本身是合法的 —— 但只在 `size == 0` 时合法。三个
    /// 适配层此前都把两种情况混成了同一个 `0.0`：**解析失败也兜 0**。
    ///
    /// 而 `entry_price = 0` 的后果是不可逆的：这份读数会成为持仓**基线**，基线每个
    /// (所, symbol) 只写一次（第二次到达判违约丢弃），此后由 Fill 在其上累加 ——
    /// [`Self::apply_fill`] 的加权均价与已实现盈亏从 0 起算，全线失真；而对账通道
    /// 只比 `size`，看不见 `entry_price` 的错误。也就是说：**错了就永远错着，且无人
    /// 报警**。宁可在解析处报错让启动失败（最便宜的失败时机）。
    ///
    /// 入参用 `Option` 表达"交易所没给/没解析出来"，由本函数按 size 判定是否合法 ——
    /// 调用方不必各自记住这条规则，也就不会各自漏掉。
    pub fn checked(
        exchange: Exchange,
        symbol: Symbol,
        size: Quantity,
        entry_price: Option<f64>,
        unrealized_pnl: Option<f64>,
    ) -> Result<Self, String> {
        let flat = size.abs() < Self::EPSILON;
        let require = |value: Option<f64>, field: &str| -> Result<f64, String> {
            match value {
                Some(v) => Ok(v),
                // 空仓：价格类字段缺失是交易所的正常表达，按 0 是如实的
                None if flat => Ok(0.0),
                None => Err(format!(
                    "{exchange} {symbol} 持仓 size={size} 非零，但 {field} 缺失或无法解析 —— \
                     它会成为持仓基线且永不可纠正（对账只比 size，发现不了）"
                )),
            }
        };
        Ok(Self {
            exchange,
            symbol: symbol.clone(),
            size,
            entry_price: require(entry_price, "entry_price")?,
            unrealized_pnl: require(unrealized_pnl, "unrealized_pnl")?,
        })
    }

    /// 应用一笔成交：维护带符号仓位与**持仓均价**，返回已实现盈亏（不含手续费）。
    ///
    /// 仓位随成交演进的规则只此一处（sim 的 `Ledger` 与策略侧的 `SymbolState` 共用；
    /// 此前策略侧只累加 size，`entry_price` 永远停在首笔值，拿它算止盈/风控的策略
    /// 读到的是烂数据）：
    /// - 新开 / 同向加仓：加权平均成本，已实现盈亏为 0
    /// - 反向平仓：平掉 min(本次, 持仓)，返回已实现盈亏，均价不变（清仓则归零）
    /// - 反手：平掉原仓（返回其已实现盈亏），剩余在成交价重新开仓
    ///
    /// 本方法不触碰 `unrealized_pnl` —— 浮盈需要估值价格，由持有方按自己的行情来源
    /// 维护（`Ledger::open_positions` 按 mark 回填；`SymbolState` 以最新成交价近似）。
    pub fn apply_fill(&mut self, side: Side, price: f64, qty: Quantity) -> f64 {
        let signed = match side {
            Side::Long => qty,
            Side::Short => -qty,
        };
        let old_size = self.size;
        let new_size = old_size + signed;

        if old_size.abs() < Self::EPSILON || (old_size > 0.0) == (signed > 0.0) {
            // 新开 / 同向加仓：加权平均
            self.entry_price = if old_size.abs() < Self::EPSILON {
                price
            } else {
                (old_size.abs() * self.entry_price + qty * price) / (old_size.abs() + qty)
            };
            self.size = new_size;
            0.0
        } else {
            // 反向：平仓 (可能反手)
            let close_qty = qty.min(old_size.abs());
            let dir = if old_size > 0.0 { 1.0 } else { -1.0 };
            let realized = (price - self.entry_price) * close_qty * dir;
            self.entry_price = if signed.abs() <= old_size.abs() {
                if new_size.abs() < Self::EPSILON {
                    0.0
                } else {
                    self.entry_price
                }
            } else {
                price // 反手: 剩余在成交价重开
            };
            self.size = new_size;
            realized
        }
    }
}

#[cfg(test)]
mod checked_tests {
    use super::*;

    /// **Critical 回归防线**：持仓非零时，价格类字段缺失/解析失败必须报错。
    ///
    /// 兜 0 的后果不可逆：这份读数会成为持仓基线（每 (所,symbol) 只写一次，第二次
    /// 判违约丢弃），此后 apply_fill 的加权均价与已实现盈亏全线失真；而对账只比 size，
    /// 永远发现不了。三个适配层（OKX 空串、HL null、IBKR 缺键）共用本规则。
    #[test]
    fn non_flat_position_rejects_missing_price_fields() {
        let err = Position::checked(Exchange::OKX, "BTC".into(), 1.5, None, Some(0.0))
            .expect_err("持仓非零而均价缺失被静默兜成了 0 —— 基线从此永久失真");
        assert!(err.contains("entry_price"), "错误应指明是哪个字段: {err}");

        assert!(
            Position::checked(Exchange::OKX, "BTC".into(), -1.5, Some(100.0), None).is_err(),
            "浮盈缺失同样不可兜 0（空头也一样）"
        );
    }

    /// 空仓时价格字段缺失是交易所的正常表达（OKX 空串 / HL null / IBKR 无该键），按 0 如实
    #[test]
    fn flat_position_allows_missing_price_fields() {
        let p = Position::checked(Exchange::Binance, "ETH".into(), 0.0, None, None).unwrap();
        assert_eq!(p.entry_price, 0.0);
        assert_eq!(p.unrealized_pnl, 0.0);
        assert!(p.is_empty());
    }

    /// 字段齐全时原样构造
    #[test]
    fn present_fields_are_taken_as_is() {
        let p = Position::checked(Exchange::IBKR, "AAPL".into(), 10.0, Some(220.5), Some(-3.0))
            .unwrap();
        assert_eq!(p.entry_price, 220.5);
        assert_eq!(p.unrealized_pnl, -3.0);
        assert_eq!(p.side(), Some(Side::Long));
    }
}
