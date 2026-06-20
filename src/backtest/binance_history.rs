use crate::backtest::data_cache::LocalFsDataCache;
use crate::backtest::downloader::BinanceHistoryDownloader;
use crate::backtest::source::MarketDataSource;
use crate::backtest::trade_print_bbo::TradePrintBboSource;
use crate::backtest::{BinanceDataKind, BinanceHistorySource};
use crate::domain::{ExchangeError, Symbol};
use chrono::NaiveDate;

/// Binance 历史行情数据组装入口 —— 回测层统一负责**下载/缓存/组装**。
///
/// 默认产出真实 trades 流 (trade-native，撮合走 [`crate::sim::SimState`] 的 trade-print 撮合)。
/// `synthesize_bbo=true` 时把 trade 替换为零价差 BBO ([`TradePrintBboSource`])，仅供写死依赖 BBO
/// 的策略且行情无 L1 时使用 (合成近似，会高估 maker 成交，默认不启用)。
pub struct BinanceHistory;

impl BinanceHistory {
    /// `symbols` 为内部基础符号 (如 "ETH")，`quote` 为计价币 (如 "USDT")，二者经 `to_binance`
    /// 还原为币安文件符号；事件统一打内部符号。
    pub fn source(
        symbols: &[Symbol],
        quote: &str,
        start: NaiveDate,
        end: NaiveDate,
        synthesize_bbo: bool,
        cache_dir: &str,
        kinds: &[BinanceDataKind],
    ) -> Result<Box<dyn MarketDataSource>, ExchangeError> {
        let cache = LocalFsDataCache::new(cache_dir);
        let downloader = BinanceHistoryDownloader::new(Box::new(cache))?;
        let base = BinanceHistorySource::new(
            downloader,
            symbols.to_vec(),
            quote.to_string(),
            start,
            end,
            kinds.to_vec(),
        );
        if synthesize_bbo {
            Ok(Box::new(TradePrintBboSource::new(base)))
        } else {
            Ok(Box::new(base))
        }
    }
}
