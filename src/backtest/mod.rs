//! 虚拟时间回测引擎 + 历史数据源。
//!
//! 与实盘共享全部领域逻辑 (撮合 [`crate::sim::SimState`]、策略 [`crate::engine::StrategyRunner`])，
//! 差异只在驱动层：实盘是并发墙钟，回测是单线程虚拟时间优先队列。

mod binance_csv;
mod binance_history;
mod binance_history_source;
mod bs_greeks_source;
mod data_cache;
mod downloader;
mod engine;
mod source;
mod trade_print_bbo;

pub use binance_history::BinanceHistory;
pub use bs_greeks_source::{BsGreeksConfig, BsGreeksSource};
pub use binance_history_source::{BinanceDataKind, BinanceHistorySource};
pub use data_cache::{DataCache, LocalFsDataCache};
pub use downloader::BinanceHistoryDownloader;
pub use engine::{BacktestEngine, BacktestResult};
pub use source::MarketDataSource;
pub use trade_print_bbo::TradePrintBboSource;
