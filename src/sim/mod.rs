//! 撮合核心 (确定性纯状态机)：账本 + 挂单簿 + 撮合判定。
//!
//! 全部撮合逻辑都是 [`SimState`] 上的确定性转移，脱离线程/锁/延迟即可同步单测。
//! 回测引擎 ([`crate::backtest`]) 在单线程虚拟时间循环里串行驱动这些转移。

mod config;
mod ledger;
mod matcher;
mod state;

pub use config::SimConfig;
pub use ledger::Ledger;
pub use matcher::{crosses, touch_price, trade_crosses};
pub use state::{Liquidity, RestingOrder, SimState};

use crate::domain::{Exchange, Symbol, SymbolMeta};
use std::collections::HashMap;

/// 各所的计价币（柜台现金、手续费、净值都以它计，与实盘「净值 = 计价币那一份」同口径）。
/// 取自合约规格；同一所出现两种计价币报错（各所客户端只接一种计价币，正常不会发生）。
pub fn exchange_quotes(
    metas: &HashMap<(Exchange, Symbol), SymbolMeta>,
) -> Result<HashMap<Exchange, String>, String> {
    let mut quotes: HashMap<Exchange, String> = HashMap::new();
    for m in metas.values() {
        match quotes.get(&m.exchange) {
            Some(q) if *q != m.quote => {
                return Err(format!(
                    "{} 的合约有两种计价币（{q}、{}），模拟柜台的现金只能是一种",
                    m.exchange, m.quote
                ))
            }
            Some(_) => {}
            None => {
                quotes.insert(m.exchange, m.quote.clone());
            }
        }
    }
    Ok(quotes)
}
