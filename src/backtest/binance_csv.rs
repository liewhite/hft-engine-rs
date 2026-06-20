//! data.binance.vision CSV (zip) 解析。
//!
//! 官方列定义 (U 本位合约)：
//!   - bookTicker: `update_id, best_bid_price, best_bid_qty, best_ask_price, best_ask_qty, transaction_time, event_time`
//!   - trades:     `id, price, qty, quote_qty, time, is_buyer_maker`
//!
//! 部分文件首行为表头：通过"首字段能否解析为数字"探测，非数字即表头跳过。
//! BBO 时间戳取 event_time (col 6) 以对齐实盘 WS 推送语义。历史事件 local_ts == exchange_ts
//! (源数据无网络延迟，延迟由回测引擎建模)，保证确定性。

use crate::domain::{Exchange, ExchangeError, MarketTrade, Symbol, BBO};
use crate::messaging::{ExchangeEventData, IncomeEvent};
use std::io::{BufRead, BufReader, Cursor};

/// 解析 bookTicker zip 字节为 BBO 事件。
pub fn parse_book_ticker(symbol: &Symbol, zip_bytes: &[u8]) -> Result<Vec<IncomeEvent>, ExchangeError> {
    map_rows(zip_bytes, |f| book_ticker_row(symbol, f))
}

/// 解析 trades zip 字节为 MarketTrade 事件。
pub fn parse_trades(symbol: &Symbol, zip_bytes: &[u8]) -> Result<Vec<IncomeEvent>, ExchangeError> {
    map_rows(zip_bytes, |f| trade_row(symbol, f))
}

fn book_ticker_row(symbol: &Symbol, f: &[&str]) -> Result<IncomeEvent, ExchangeError> {
    let bbo = BBO {
        exchange: Exchange::Binance,
        symbol: symbol.clone(),
        bid_price: field_f64(f, 1)?,
        bid_qty: field_f64(f, 2)?,
        ask_price: field_f64(f, 3)?,
        ask_qty: field_f64(f, 4)?,
        timestamp: field_u64(f, 6)?,
    };
    Ok(historical(bbo.timestamp, ExchangeEventData::BBO(bbo)))
}

fn trade_row(symbol: &Symbol, f: &[&str]) -> Result<IncomeEvent, ExchangeError> {
    let trade = MarketTrade {
        exchange: Exchange::Binance,
        symbol: symbol.clone(),
        price: field_f64(f, 1)?,
        qty: field_f64(f, 2)?,
        is_buyer_maker: f.get(5).map(|s| s.trim().eq_ignore_ascii_case("true")).unwrap_or(false),
        timestamp: field_u64(f, 4)?,
    };
    Ok(historical(trade.timestamp, ExchangeEventData::MarketTrade(trade)))
}

/// 历史事件构造：local_ts == exchange_ts。
fn historical(ts: u64, data: ExchangeEventData) -> IncomeEvent {
    IncomeEvent {
        exchange_ts: ts,
        local_ts: ts,
        data,
    }
}

/// 解压 zip 内单一 CSV，**逐行流式**切分字段并映射；自动跳过表头行 (首字段非数字)。
///
/// 用 `BufReader` 逐行读取解压流 (不一次性物化整份 CSV 字符串)，峰值内存 = 压缩 zip + 结果
/// Vec + 单行，避免整月大文件 OOM。
fn map_rows(
    zip_bytes: &[u8],
    mut f: impl FnMut(&[&str]) -> Result<IncomeEvent, ExchangeError>,
) -> Result<Vec<IncomeEvent>, ExchangeError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(zip_bytes))
        .map_err(|e| ExchangeError::ParseError(format!("open zip: {e}")))?;
    if archive.is_empty() {
        return Ok(Vec::new());
    }
    let entry = archive
        .by_index(0)
        .map_err(|e| ExchangeError::ParseError(format!("zip entry: {e}")))?;
    let reader = BufReader::new(entry);

    let mut out = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(|e| ExchangeError::ParseError(format!("read csv line: {e}")))?;
        if line.is_empty() || !is_data_row(&line) {
            continue;
        }
        let fields: Vec<&str> = line.split(',').collect();
        out.push(f(&fields)?);
    }
    Ok(out)
}

/// 数据行判定：首字段 (update_id / id) 可解析为数字即数据行，否则为表头。
fn is_data_row(line: &str) -> bool {
    let head = line.split(',').next().unwrap_or("");
    !head.is_empty() && head.bytes().all(|c| c.is_ascii_digit())
}

fn field_f64(f: &[&str], idx: usize) -> Result<f64, ExchangeError> {
    let raw = f
        .get(idx)
        .ok_or_else(|| ExchangeError::ParseError(format!("missing column {idx}")))?;
    raw.trim()
        .parse::<f64>()
        .map_err(|e| ExchangeError::ParseError(format!("col {idx} '{raw}' as f64: {e}")))
}

fn field_u64(f: &[&str], idx: usize) -> Result<u64, ExchangeError> {
    let raw = f
        .get(idx)
        .ok_or_else(|| ExchangeError::ParseError(format!("missing column {idx}")))?;
    raw.trim()
        .parse::<u64>()
        .map_err(|e| ExchangeError::ParseError(format!("col {idx} '{raw}' as u64: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    fn zip_with(csv: &str) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(Cursor::new(&mut buf));
            zip.start_file("data.csv", SimpleFileOptions::default()).unwrap();
            zip.write_all(csv.as_bytes()).unwrap();
            zip.finish().unwrap();
        }
        buf
    }

    #[test]
    fn parse_trades_skips_header() {
        let csv = "id,price,qty,quote_qty,time,is_buyer_maker\n\
                   1,100.5,2.0,201.0,1700000000000,true\n\
                   2,101.0,1.0,101.0,1700000000100,false\n";
        let evs = parse_trades(&"BTCUSDT".to_string(), &zip_with(csv)).unwrap();
        assert_eq!(evs.len(), 2);
        match &evs[0].data {
            ExchangeEventData::MarketTrade(t) => {
                assert_eq!(t.price, 100.5);
                assert_eq!(t.qty, 2.0);
                assert!(t.is_buyer_maker);
                assert_eq!(t.timestamp, 1700000000000);
                assert_eq!(evs[0].exchange_ts, 1700000000000);
                assert_eq!(evs[0].local_ts, 1700000000000);
            }
            _ => panic!("expected MarketTrade"),
        }
        match &evs[1].data {
            ExchangeEventData::MarketTrade(t) => assert!(!t.is_buyer_maker),
            _ => panic!("expected MarketTrade"),
        }
    }

    #[test]
    fn parse_book_ticker_event_time_col() {
        // update_id, bid, bidqty, ask, askqty, transaction_time, event_time
        let csv = "100,99.0,1.0,101.0,2.0,1700000000000,1700000000050\n";
        let evs = parse_book_ticker(&"BTCUSDT".to_string(), &zip_with(csv)).unwrap();
        assert_eq!(evs.len(), 1);
        match &evs[0].data {
            ExchangeEventData::BBO(b) => {
                assert_eq!(b.bid_price, 99.0);
                assert_eq!(b.ask_price, 101.0);
                assert_eq!(b.timestamp, 1700000000050); // event_time
            }
            _ => panic!("expected BBO"),
        }
    }
}
