use crate::backtest::binance_csv;
use crate::backtest::downloader::BinanceHistoryDownloader;
use crate::backtest::source::MarketDataSource;
use crate::domain::Symbol;
use crate::exchange::binance::to_binance;
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

    fn parse(
        self,
        symbol: &Symbol,
        zip_bytes: &[u8],
    ) -> Result<Vec<IncomeEvent>, crate::domain::ExchangeError> {
        match self {
            BinanceDataKind::BookTicker => binance_csv::parse_book_ticker(symbol, zip_bytes),
            BinanceDataKind::Trades => binance_csv::parse_trades(symbol, zip_bytes),
        }
    }
}

/// 币安历史数据源：把指定 symbols、日期区间 [start, end] 的若干 [`BinanceDataKind`] 还原为
/// 全局时间有序的事件流。
///
/// **惰性按天加载**：`events()` 返回的迭代器逐日加载——单日内所有 symbol×kind 解析后合并、
/// 按时间戳**稳定排序** (同时间戳保持文件内相对序 -> 确定性) 再产出，该日产完即丢弃。峰值内存
/// = 单日数据量 (整月 ETH 全量物化会 OOM，故必须按天流式)。
///
/// 错误处理：下载/解析失败在迭代时 **panic fail-fast** (带 key/原因上下文)——回测是离线一次性
/// 任务，宁可响亮中断也不静默漏数据；缺某日数据 (404) 则跳过该日 (该日无成交)。
///
/// 符号约定：`symbols` 为**内部基础符号** (如 "ETH"，与 StateManager/SymbolMeta 一致)；文件路径
/// 用 `to_binance(symbol, quote)` 还原为币安符号 (如 "ETHUSDT")，事件则统一打内部符号 -> 与实盘
/// 行情流同构。
pub struct BinanceHistorySource {
    downloader: BinanceHistoryDownloader,
    symbols: Vec<Symbol>,
    quote: String,
    start: NaiveDate,
    end: NaiveDate,
    kinds: Vec<BinanceDataKind>,
}

impl BinanceHistorySource {
    pub fn new(
        downloader: BinanceHistoryDownloader,
        symbols: Vec<Symbol>,
        quote: String,
        start: NaiveDate,
        end: NaiveDate,
        kinds: Vec<BinanceDataKind>,
    ) -> Self {
        Self {
            downloader,
            symbols,
            quote,
            start,
            end,
            kinds,
        }
    }

    /// 加载单日事件 (合并 symbol×kind、稳定按时间戳排序)。下载/解析失败 panic fail-fast。
    fn load_day(&self, date: NaiveDate) -> Vec<IncomeEvent> {
        let date_str = date.format("%Y-%m-%d").to_string();
        let mut day_events = Vec::new();
        for symbol in &self.symbols {
            let binance_symbol = to_binance(symbol, &self.quote);
            for kind in &self.kinds {
                let key = BinanceHistoryDownloader::daily_key(kind.key(), &binance_symbol, &date_str);
                let bytes = self
                    .downloader
                    .fetch(&key)
                    .unwrap_or_else(|e| panic!("fetch {key} failed: {e}"));
                if let Some(bytes) = bytes {
                    // 解析时打**内部符号** (非币安符号) -> 与 StateManager/策略订阅一致
                    let parsed = kind
                        .parse(symbol, &bytes)
                        .unwrap_or_else(|e| panic!("parse {key} failed: {e}"));
                    day_events.extend(parsed);
                }
            }
        }
        // 稳定排序: 同时间戳保持文件内相对序 -> 确定性
        day_events.sort_by_key(|e| e.exchange_ts);
        tracing::info!(date = %date_str, count = day_events.len(), "loaded day");
        day_events
    }

    fn date_range(&self) -> Vec<NaiveDate> {
        let mut dates = Vec::new();
        let mut d = self.start;
        while d <= self.end {
            dates.push(d);
            d = match d.succ_opt() {
                Some(n) => n,
                None => break,
            };
        }
        dates
    }
}

impl MarketDataSource for BinanceHistorySource {
    fn events(&self) -> Box<dyn Iterator<Item = IncomeEvent> + '_> {
        Box::new(
            self.date_range()
                .into_iter()
                .flat_map(move |date| self.load_day(date).into_iter()),
        )
    }
}
