use crate::backtest::source::MarketDataSource;
use crate::domain::BBO;
use crate::messaging::{ExchangeEventData, IncomeEvent};

/// 把成交印记 (trades) 还原为合成零价差 L1 行情的数据源装饰器。
///
/// **可选/旁路功能** (非默认)：仅当策略写死依赖 BBO、而行情只有 trades 时才启用。它把每条
/// `MarketTrade` **替换**为 bid=ask=成交价的 `BBO`，其余事件原样透传。spread=0 是合成近似，
/// 会高估 maker 成交。默认回测路径**不启用** —— 撮合直接用真实 trade ([`crate::sim::SimState`] 的
/// trade-print 撮合)。
///
/// 假定上游**没有真实 bookTicker** (否则会与合成 L1 冲突)。
pub struct TradePrintBboSource<S: MarketDataSource> {
    underlying: S,
}

impl<S: MarketDataSource> TradePrintBboSource<S> {
    pub fn new(underlying: S) -> Self {
        Self { underlying }
    }
}

impl<S: MarketDataSource> MarketDataSource for TradePrintBboSource<S> {
    fn events(&self) -> Box<dyn Iterator<Item = IncomeEvent> + '_> {
        Box::new(self.underlying.events().map(|ev| match ev.data {
            ExchangeEventData::MarketTrade(t) => IncomeEvent {
                exchange_ts: ev.exchange_ts,
                local_ts: ev.local_ts,
                data: ExchangeEventData::BBO(BBO {
                    exchange: t.exchange,
                    symbol: t.symbol,
                    bid_price: t.price,
                    bid_qty: t.qty,
                    ask_price: t.price,
                    ask_qty: t.qty,
                    timestamp: t.timestamp,
                }),
            },
            _ => ev,
        }))
    }
}
