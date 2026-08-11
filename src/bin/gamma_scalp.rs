//! 期权 gamma scalping 回测入口 ([`GammaScalpStrategy`])。
//!
//! 数据：Binance ETHUSDT 永续历史 trades (trade-native，撮合走真实 trade 越价)。期权希腊字母为
//! **mock**：用 [`BsGreeksSource`] 以 Black-Scholes 合成一份长 ATM 跨式 (N call + N put，行权价取
//! 回测起点价) 的账户级 greeks，喂入与实盘 OKX 同一 Greeks 通道——策略代码与实盘完全相同。
//!
//! 完整 P&L = 期权腿 (跨式 BS 估值变化，含 theta) + 永续对冲腿 (含未实现 + 手续费)。
//!
//! 运行: cargo run --release --bin gamma_scalp -- [START yyyy-mm-dd] [END yyyy-mm-dd]
//! 缺省 [2026-05-01 .. 2026-05-31]。回测结束打印汇总并把 PnL 时序写入 JSON 供画图。

use anyhow::Context;
use chrono::{NaiveDate, NaiveTime, TimeZone, Utc};
use hft_engine_rs::backtest::{
    BacktestEngine, BinanceDataKind, BinanceHistory, BsGreeksConfig, BsGreeksSource,
};
use hft_engine_rs::domain::{Exchange, Symbol, SymbolMeta};
use hft_engine_rs::engine::{SequentialClientOrderIdGen, StrategyRunner};
use hft_engine_rs::exchange::binance::BinanceClient;
use hft_engine_rs::exchange::ExchangeClient;
use hft_engine_rs::messaging::{AccountData, IncomeEvent, MarketData};
use hft_engine_rs::sim::SimConfig;
use hft_engine_rs::strategy::GammaScalpStrategy;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

const EX: Exchange = Exchange::Binance;

/// PnL 采样器：跟踪最新成交价，并在每个 AccountInfo (按时钟周期) 记一帧对冲腿净值。
///
/// 注：每帧价取「该小时标记前的最近一笔成交价」，ts 取标记时刻——成交极密 (整月 1.2 亿笔)，
/// 二者错配在毫秒级，对小时级 PnL 曲线可忽略。
#[derive(Default)]
struct Sampler {
    last_price: f64,
    /// (ts, 对冲腿 equity, 当时标的价)
    samples: Vec<(u64, f64, f64)>,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                // 成交量大：默认压到 warn，仅放行回测模块的进度 (loaded day / backtest done)
                .unwrap_or_else(|_| {
                    tracing_subscriber::EnvFilter::new("warn,hft_engine_rs::backtest=info")
                }),
        )
        .init();

    // ==================== 配置 ====================
    // 内部符号为基础币 (ETH)，与 SymbolMeta / StateManager 一致；计价币单独给数据源还原文件名
    let symbol: Symbol = "ETH".to_string();
    let quote = "USDT";
    let ccy = "ETH".to_string();
    let implied_vol = 0.6; // 年化 IV (ETH 典型)
    let risk_free_rate = 0.0;
    let straddles = 10.0; // 长跨式份数
    let delta_band = 0.1; // 净 delta 对称容忍带 (ETH)
    let base_offset_ratio = 0.002; // 对冲间距 0.2% (PostOnly 距最新成交价)
    let maker_fee_rate = 0.0002; // gamma scalp 核心成本 (PostOnly 对冲单)
    let taker_fee_rate = 0.0005;
    let initial_balance_usdt = 100_000.0;
    let clock_interval_ms = 3_600_000; // 1h 刷新净值 -> PnL 曲线按小时采样

    let args: Vec<String> = std::env::args().skip(1).collect();
    let start_str = args.first().cloned().unwrap_or_else(|| "2026-05-01".to_string());
    let end_str = args.get(1).cloned().unwrap_or_else(|| "2026-05-31".to_string());
    let start = NaiveDate::parse_from_str(&start_str, "%Y-%m-%d").context("parse START")?;
    let end = NaiveDate::parse_from_str(&end_str, "%Y-%m-%d").context("parse END")?;

    // 真实 Binance (无凭证) 拉取 symbol 元数据 (精度)
    let client = BinanceClient::new("USDT".to_string(), None).map_err(|e| anyhow::anyhow!("binance client: {e}"))?;
    let metas_vec = tokio::runtime::Runtime::new()?
        .block_on(client.fetch_all_symbol_metas())
        .map_err(|e| anyhow::anyhow!("fetch symbol metas: {e}"))?;
    let symbol_metas: Arc<HashMap<(Exchange, Symbol), SymbolMeta>> = Arc::new(
        metas_vec
            .into_iter()
            .map(|m| ((m.exchange, m.symbol.clone()), m))
            .collect(),
    );

    // trade-native 数据源 (按天惰性流式，峰值=单日)
    let trade_source = BinanceHistory::source(
        std::slice::from_ref(&symbol),
        quote,
        start,
        end,
        false,
        "data-cache",
        &[BinanceDataKind::AggTrades],
    )
    .map_err(|e| anyhow::anyhow!("build history source: {e}"))?;

    // 单只 ATM 长跨式，到期 = 回测结束日+1 (tenor ≈ 回测周期，全程持有不滚动)
    let expiry_dt = Utc.from_utc_datetime(
        &end.succ_opt()
            .context("end date overflow")?
            .and_time(NaiveTime::MIN),
    );
    let expiry_ms = expiry_dt.timestamp_millis() as u64;
    let greeks_config = BsGreeksConfig {
        exchange: EX,
        ccy: ccy.clone(),
        underlying_symbol: symbol.clone(),
        straddles,
        implied_vol,
        expiry: expiry_ms,
        risk_free_rate,
        spot_holding: 0.0,
        emit_interval_ms: 1000,
        min_tenor_days: 1.0,
        strangle_width_pct: 0.0,
    };
    let source = BsGreeksSource::new(trade_source, greeks_config);

    let strategy = GammaScalpStrategy::new(EX, symbol.clone(), ccy.clone(), delta_band, base_offset_ratio);
    let runner = StrategyRunner::with_id_gen(
        Box::new(strategy),
        symbol_metas.clone(),
        Box::new(SequentialClientOrderIdGen::default()),
    );
    let config = SimConfig {
        exchange_to_strategy_delay_ms: 100,
        order_to_exchange_delay_ms: 50,
        initial_balance_usdt,
        maker_fee_rate,
        taker_fee_rate,
    };

    let sampler: Rc<RefCell<Sampler>> = Rc::new(RefCell::new(Sampler::default()));
    let sampler_obs = Rc::clone(&sampler);

    // 借用 source 的引擎放内层作用域，run 后释放借用以便查询 option_pnl
    let result = {
        let mut engine = BacktestEngine::new(&source, vec![runner], config, symbol_metas)
            .with_clock_interval(clock_interval_ms)
            .with_observer(move |ev: &IncomeEvent| {
                let mut s = sampler_obs.borrow_mut();
                match ev {
                    IncomeEvent::Market(m) => {
                        if let MarketData::MarketTrade(t) = &m.data {
                            s.last_price = t.price;
                        }
                    }
                    IncomeEvent::Account(a) => {
                        if let AccountData::AccountInfo { equity, .. } = &a.data {
                            // 首笔成交之前 (price=0) 不采样
                            if s.last_price > 0.0 {
                                let p = s.last_price;
                                s.samples.push((a.exchange_ts, *equity, p));
                            }
                        }
                    }
                }
            });
        engine.run()
    };

    // ==================== P&L 拆解 ====================
    // 终点口径统一：用引擎最终虚拟时刻 result.last_ts + 最新成交价，并把这一真实终点帧追加到
    // 时序——保证「汇总 total == 曲线终点」是构造出来的，而非两套时间戳碰巧相等。
    let mut s = sampler.borrow_mut();
    let strike = source.strike();
    let last_price = s.last_price;
    let final_ts = result.last_ts;
    if last_price > 0.0 {
        s.samples.push((final_ts, result.final_equity, last_price));
    }
    let option_pnl = source.option_pnl(last_price, final_ts);
    let hedge_pnl = result.final_equity - result.initial_balance;
    let total_pnl = hedge_pnl + option_pnl;
    let days = (end - start).num_days() + 1;

    println!("==================== GammaScalp Backtest Result ====================");
    println!("symbol         : {symbol}  [{start} .. {end}] ({days} days)");
    println!(
        "期权            : 起点ATM {strike:.2} | IV={implied_vol} straddles={straddles} \
         band={delta_band} ETH (持有到期)"
    );
    println!("对冲间距       : {:.2}% (PostOnly 对冲单, 对称)", base_offset_ratio * 100.0);
    println!("maker fee      : {:.3}%", maker_fee_rate * 100.0);
    println!(
        "start/end px    : {strike:.2} -> {last_price:.2}  ({:+.2}%)",
        (last_price / strike - 1.0) * 100.0
    );
    println!("market events  : {}", result.market_events);
    println!("fills          : {}", result.fills);
    println!("-------------------- P&L 拆解 (USDT) --------------------");
    println!("  期权腿 MTM   : {option_pnl:+.2}   (长跨式 BS 估值变化, 含 theta)");
    println!("  永续对冲腿   : {hedge_pnl:+.2}   (含未实现 + 手续费)");
    println!("  完整 gamma   : {total_pnl:+.2}   (期权腿 + 对冲腿)");
    println!("---------------------------------------------------------");
    println!("对冲腿 realized : {:.2} USDT (已扣手续费) | 末仓 {:?}", result.realized_pnl, result.positions);

    // ==================== 写 PnL 时序 JSON (供画图) ====================
    let out_path = std::env::var("GAMMA_PNL_OUT")
        .unwrap_or_else(|_| "gamma_pnl.json".to_string());
    let mut json = String::from("[");
    for (i, &(ts, equity, price)) in s.samples.iter().enumerate() {
        let hedge = equity - result.initial_balance;
        let opt = source.option_pnl(price, ts);
        let total = hedge + opt;
        let iso = Utc
            .timestamp_millis_opt(ts as i64)
            .single()
            .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
            .unwrap_or_default();
        if i > 0 {
            json.push(',');
        }
        json.push_str(&format!(
            "{{\"ts\":{ts},\"iso\":\"{iso}\",\"price\":{price:.4},\"hedge_pnl\":{hedge:.4},\"option_pnl\":{opt:.4},\"total_pnl\":{total:.4}}}"
        ));
    }
    json.push(']');
    std::fs::write(&out_path, json).context("write pnl json")?;
    println!("PnL 时序 ({} 帧) 已写入: {out_path}", s.samples.len());
    println!("====================================================================");
    Ok(())
}
