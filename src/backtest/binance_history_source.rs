use crate::backtest::binance_csv;
use crate::backtest::downloader::BinanceHistoryDownloader;
use crate::backtest::source::MarketDataSource;
use crate::domain::{ExchangeError, Symbol};
use crate::messaging::IncomeEvent;
use chrono::NaiveDate;

/// 币安每日历史数据类型：自带数据源路径 key 段与对应解析器。
/// trade-native 回测只需 [`BinanceDataKind::Trades`]，避免去取已停发/无需的 bookTicker。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinanceDataKind {
    BookTicker,
    Trades,
}

impl BinanceDataKind {
    pub fn key(self) -> &'static str {
        match self {
            BinanceDataKind::BookTicker => "bookTicker",
            BinanceDataKind::Trades => "trades",
        }
    }

    fn parse(self, symbol: &Symbol, zip_bytes: &[u8]) -> Result<Vec<IncomeEvent>, ExchangeError> {
        match self {
            BinanceDataKind::BookTicker => binance_csv::parse_book_ticker(symbol, zip_bytes),
            BinanceDataKind::Trades => binance_csv::parse_trades(symbol, zip_bytes),
        }
    }
}

/// 币安历史数据源：把指定 symbols、日期区间 [start, end] 的若干 [`BinanceDataKind`] 还原为
/// 全局时间有序的事件流。
///
/// **加载策略**：构造时 (`load`) 按天下载+解析，单日内所有 symbol×kind 合并后按时间戳**稳定排序**
/// (同时间戳保持文件内相对序 -> 确定性)，逐天串联物化到内存。download/parse 的失败在此 fail-fast
/// 向上传播 (不静默吞错)，故 `events()` 本身无误。
///
/// 注：相比 ox-demo 的逐日惰性加载 (内存上界 = 单日)，此实现整段物化 (内存 = 整区间)。换取
/// 错误处理与 `MarketDataSource` 契约的简洁；超大区间可后续改为分块迭代器而不动接口。
pub struct BinanceHistorySource {
    events: Vec<IncomeEvent>,
}

impl BinanceHistorySource {
    pub fn load(
        downloader: &BinanceHistoryDownloader,
        symbols: &[Symbol],
        start: NaiveDate,
        end: NaiveDate,
        kinds: &[BinanceDataKind],
    ) -> Result<Self, ExchangeError> {
        let mut events = Vec::new();
        let mut date = start;
        while date <= end {
            let date_str = date.format("%Y-%m-%d").to_string();
            let mut day_events = Vec::new();
            for symbol in symbols {
                for kind in kinds {
                    let key = BinanceHistoryDownloader::daily_key(kind.key(), symbol, &date_str);
                    if let Some(bytes) = downloader.fetch(&key)? {
                        day_events.extend(kind.parse(symbol, &bytes)?);
                    }
                }
            }
            // 稳定排序: 同时间戳保持文件内相对序 -> 确定性
            day_events.sort_by_key(|e| e.exchange_ts);
            tracing::info!(
                date = %date_str,
                count = day_events.len(),
                "loaded day"
            );
            events.extend(day_events);
            date = date.succ_opt().ok_or_else(|| {
                ExchangeError::Other("date overflow while iterating range".to_string())
            })?;
        }
        Ok(Self { events })
    }
}

impl MarketDataSource for BinanceHistorySource {
    fn events(&self) -> Box<dyn Iterator<Item = IncomeEvent> + '_> {
        Box::new(self.events.iter().cloned())
    }
}
