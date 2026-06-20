//! 回测用 BS 合成希腊字母数据源装饰器 (单只 ATM 跨式，持有到期，不滚动)。
//!
//! 监听上游标的 [`MarketTrade`](crate::domain::MarketTrade) (真实逐笔成交价 S)，首笔成交开一份
//! ATM 长跨式 (long call + long put)，按虚拟时间间隔聚合为与 OKX `account/greeks` 同形态的
//! per-ccy 账户级 [`Greeks`]，紧随该 trade 以**相同 exchange_ts** 注入 GreeksUpdate；并一次性注入
//! 现货 cashBal ([`Balance`]) 使 StateManager 的 delta 修正生效。
//!
//! 单位约定同 Greeks 通道：theta 每日、vega 对 1%。
//! (移植自 ox-demo `hft.backtest.BsGreeksSource`)

use crate::backtest::source::MarketDataSource;
use crate::domain::{Balance, Exchange, Greeks, Symbol, Timestamp};
use crate::messaging::{ExchangeEventData, IncomeEvent};
use crate::option::{self, OptionRight, MILLIS_PER_YEAR};
use std::cell::RefCell;

/// 与 [`MILLIS_PER_YEAR`] 一致的天数基准，用于 theta 每年->每日换算
const DAYS_PER_YEAR: f64 = 365.0;

/// 单只 ATM 跨式的 BS 合成希腊字母配置 (不滚动，持有到 `expiry`)。
#[derive(Debug, Clone)]
pub struct BsGreeksConfig {
    pub exchange: Exchange,
    pub ccy: String,
    pub underlying_symbol: Symbol,
    /// 跨式份数 (N 份 = long N call + N put ATM)
    pub straddles: f64,
    pub implied_vol: f64,
    /// 到期时间 (ms epoch)，回测中设为回测结束日 -> tenor ≈ 回测周期
    pub expiry: Timestamp,
    pub risk_free_rate: f64,
    /// 现货持仓 (delta 修正用)，纯期权+永续对冲时为 0
    pub spot_holding: f64,
    /// 希腊字母发射间隔 (虚拟时间 ms)，默认 1000 对齐实盘 OKX 轮询节奏
    pub emit_interval_ms: u64,
    /// 剩余期限下限 (天)：临近到期按此钳制，避免到期日 ATM gamma/theta 奇点 (T->0 发散)
    pub min_tenor_days: f64,
    /// 宽跨价外宽度：0=ATM 跨式；>0 时 call 行权=首价·(1+w)、put 行权=首价·(1−w)
    pub strangle_width_pct: f64,
}

impl Default for BsGreeksConfig {
    fn default() -> Self {
        Self {
            exchange: Exchange::Binance,
            ccy: String::new(),
            underlying_symbol: String::new(),
            straddles: 1.0,
            implied_vol: 0.6,
            expiry: 0,
            risk_free_rate: 0.0,
            spot_holding: 0.0,
            emit_interval_ms: 1000,
            min_tenor_days: 1.0,
            strangle_width_pct: 0.0,
        }
    }
}

/// 首笔成交时锁定的期权结构 (行权价 + 进场权利金)。
#[derive(Debug, Clone, Copy, Default)]
struct StraddleState {
    strike: f64,      // ATM 参考 (首笔成交价)
    call_strike: f64, // 跨式=strike; 宽跨=strike·(1+w)
    put_strike: f64,  // 跨式=strike; 宽跨=strike·(1−w)
    entry_premium: f64,
    inited: bool,
}

pub struct BsGreeksSource<S: MarketDataSource> {
    underlying: S,
    config: BsGreeksConfig,
    /// 首笔成交时锁定、之后只读；用 RefCell 因 events(&self) 需在迭代中初始化，run 后只读查询
    state: RefCell<StraddleState>,
}

impl<S: MarketDataSource> BsGreeksSource<S> {
    pub fn new(underlying: S, config: BsGreeksConfig) -> Self {
        Self {
            underlying,
            config,
            state: RefCell::new(StraddleState::default()),
        }
    }

    /// 剩余年限，带下限钳制 (避免到期日 gamma/theta 奇点)
    fn t_years(&self, now: Timestamp) -> f64 {
        let raw = (self.config.expiry as f64 - now as f64) / MILLIS_PER_YEAR;
        raw.max(self.config.min_tenor_days / DAYS_PER_YEAR)
    }

    /// 期权结构 (跨式/宽跨) 在 (s, now) 的理论价值 (call@call_strike + put@put_strike)
    fn straddle_value(&self, st: &StraddleState, s: f64, now: Timestamp) -> f64 {
        let t_y = self.t_years(now);
        let call = option::greeks(OptionRight::Call, s, st.call_strike, t_y, self.config.implied_vol, self.config.risk_free_rate);
        let put = option::greeks(OptionRight::Put, s, st.put_strike, t_y, self.config.implied_vol, self.config.risk_free_rate);
        self.config.straddles * (call.price + put.price)
    }

    /// 期权结构聚合为账户级 Greeks (theta 每日、vega 对 1%)
    fn greeks_at(&self, st: &StraddleState, s: f64, now: Timestamp) -> Greeks {
        let t_y = self.t_years(now);
        let call = option::greeks(OptionRight::Call, s, st.call_strike, t_y, self.config.implied_vol, self.config.risk_free_rate);
        let put = option::greeks(OptionRight::Put, s, st.put_strike, t_y, self.config.implied_vol, self.config.risk_free_rate);
        let n = self.config.straddles;
        Greeks {
            exchange: self.config.exchange,
            ccy: self.config.ccy.clone(),
            delta: n * (call.delta + put.delta),
            gamma: n * (call.gamma + put.gamma),
            theta: n * (call.theta + put.theta) / DAYS_PER_YEAR, // 每年 -> 每日
            vega: n * (call.vega + put.vega) / 100.0,            // 对 1.0 -> 对 1%
            timestamp: now,
        }
    }

    /// 期权腿 P&L = 当前跨式价值 − 进场权利金 (单只持仓，供 demo 在 run 后查询)
    pub fn option_pnl(&self, s: f64, now: Timestamp) -> f64 {
        let st = *self.state.borrow();
        if !st.inited {
            return 0.0;
        }
        self.straddle_value(&st, s, now) - st.entry_premium
    }

    /// ATM 行权价 (首笔成交价)；未初始化返回 0
    pub fn strike(&self) -> f64 {
        self.state.borrow().strike
    }
}

impl<S: MarketDataSource> MarketDataSource for BsGreeksSource<S> {
    fn events(&self) -> Box<dyn Iterator<Item = IncomeEvent> + '_> {
        let mut last_emit: Option<Timestamp> = None;
        let mut balance_emitted = false;
        Box::new(self.underlying.events().flat_map(move |ev| {
            // 仅对标的逐笔成交合成希腊字母，其余事件原样透传
            let trade = match &ev.data {
                ExchangeEventData::MarketTrade(t) if t.symbol == self.config.underlying_symbol => {
                    Some((t.price, ev.exchange_ts))
                }
                _ => None,
            };
            let Some((s, now)) = trade else {
                return vec![ev];
            };

            // 首笔成交：锁定 ATM 行权价与进场权利金
            {
                let mut st = self.state.borrow_mut();
                if !st.inited {
                    let locked = StraddleState {
                        strike: s,
                        call_strike: s * (1.0 + self.config.strangle_width_pct),
                        put_strike: s * (1.0 - self.config.strangle_width_pct),
                        entry_premium: 0.0,
                        inited: true,
                    };
                    let entry = self.straddle_value(&locked, s, now);
                    *st = StraddleState {
                        entry_premium: entry,
                        ..locked
                    };
                }
            }

            let should_emit = match last_emit {
                None => true,
                Some(prev) => now.saturating_sub(prev) >= self.config.emit_interval_ms,
            };
            if !should_emit {
                return vec![ev];
            }
            last_emit = Some(now);

            let greeks = {
                let st = *self.state.borrow();
                self.greeks_at(&st, s, now)
            };
            let greeks_ev = IncomeEvent {
                exchange_ts: now,
                local_ts: now,
                data: ExchangeEventData::Greeks(greeks),
            };
            if balance_emitted {
                vec![ev, greeks_ev]
            } else {
                balance_emitted = true;
                let balance_ev = IncomeEvent {
                    exchange_ts: now,
                    local_ts: now,
                    data: ExchangeEventData::Balance(Balance {
                        exchange: self.config.exchange,
                        asset: self.config.ccy.clone(),
                        available: self.config.spot_holding,
                        frozen: 0.0,
                    }),
                };
                vec![ev, balance_ev, greeks_ev]
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::MarketTrade;
    use crate::option::{self, OptionRight};

    struct FixedSrc(Vec<IncomeEvent>);
    impl MarketDataSource for FixedSrc {
        fn events(&self) -> Box<dyn Iterator<Item = IncomeEvent> + '_> {
            Box::new(self.0.iter().cloned())
        }
    }

    fn trade_ev(price: f64, ts: u64) -> IncomeEvent {
        IncomeEvent {
            exchange_ts: ts,
            local_ts: ts,
            data: ExchangeEventData::MarketTrade(MarketTrade {
                exchange: Exchange::Binance,
                symbol: "ETH".to_string(),
                price,
                qty: 1.0,
                is_buyer_maker: false,
                timestamp: ts,
            }),
        }
    }

    fn cfg(now0: u64) -> BsGreeksConfig {
        BsGreeksConfig {
            exchange: Exchange::Binance,
            ccy: "ETH".to_string(),
            underlying_symbol: "ETH".to_string(),
            straddles: 1.0,
            implied_vol: 0.6,
            // 到期 = 首笔 + 30 天 -> tenor 干净
            expiry: now0 + 30 * 24 * 3600 * 1000,
            risk_free_rate: 0.0,
            spot_holding: 0.0,
            emit_interval_ms: 0, // 每笔都发
            min_tenor_days: 1.0,
            strangle_width_pct: 0.0,
        }
    }

    #[test]
    fn locks_strike_and_entry_zero() {
        let src = FixedSrc(vec![trade_ev(2000.0, 1000), trade_ev(2100.0, 2000)]);
        let bs = BsGreeksSource::new(src, cfg(1000));
        let evs: Vec<_> = bs.events().collect();
        // 首笔行权价锁定
        assert_eq!(bs.strike(), 2000.0);
        // 进场即估值 -> 期权腿 P&L ≈ 0
        assert!(bs.option_pnl(2000.0, 1000).abs() < 1e-9);
        // 首笔注入 trade + balance + greeks 三事件
        let balances = evs
            .iter()
            .filter(|e| matches!(&e.data, ExchangeEventData::Balance(b) if b.asset == "ETH"))
            .count();
        assert_eq!(balances, 1, "balance 只发一次");
        assert!(evs.iter().any(|e| matches!(e.data, ExchangeEventData::Greeks(_))));
    }

    #[test]
    fn greeks_unit_conversion() {
        let now0 = 1000u64;
        let src = FixedSrc(vec![trade_ev(2000.0, now0)]);
        let c = cfg(now0);
        let bs = BsGreeksSource::new(src, c.clone());
        let evs: Vec<_> = bs.events().collect();
        let g = evs
            .iter()
            .find_map(|e| match &e.data {
                ExchangeEventData::Greeks(g) => Some(g.clone()),
                _ => None,
            })
            .expect("greeks emitted");

        // 期望: per-contract 求和后, theta 每年->每日 (/365)、vega 对1.0->对1% (/100)
        let t_y = 30.0 / 365.0;
        let call = option::greeks(OptionRight::Call, 2000.0, 2000.0, t_y, 0.6, 0.0);
        let put = option::greeks(OptionRight::Put, 2000.0, 2000.0, t_y, 0.6, 0.0);
        assert!((g.delta - (call.delta + put.delta)).abs() < 1e-9);
        assert!((g.gamma - (call.gamma + put.gamma)).abs() < 1e-12);
        assert!((g.theta - (call.theta + put.theta) / 365.0).abs() < 1e-9);
        assert!((g.vega - (call.vega + put.vega) / 100.0).abs() < 1e-9);
    }
}
