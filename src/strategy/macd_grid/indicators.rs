/// Exponential Moving Average (incremental)
///
/// Warm-up: first `period` values use SMA, then switch to EMA.
/// Only uses confirmed data points.
pub struct Ema {
    period: usize,
    multiplier: f64,
    committed: Option<f64>,
    count: usize,
    sum: f64,
}

impl Ema {
    pub fn new(period: usize) -> Self {
        Self {
            period,
            multiplier: 2.0 / (period as f64 + 1.0),
            committed: None,
            count: 0,
            sum: 0.0,
        }
    }

    /// Commit a confirmed data point, advancing internal state
    pub fn update(&mut self, value: f64) {
        self.count += 1;
        if self.count < self.period {
            self.sum += value;
        } else if self.count == self.period {
            self.sum += value;
            self.committed = Some(self.sum / self.period as f64);
        } else {
            let prev = self.committed.unwrap();
            self.committed = Some(prev + self.multiplier * (value - prev));
        }
    }

    pub fn value(&self) -> Option<f64> {
        self.committed
    }

    pub fn is_ready(&self) -> bool {
        self.committed.is_some()
    }
}

/// MACD Calculator (standard 12/26/9)
///
/// - MACD line = fast EMA(12) - slow EMA(26)
/// - Signal line (DEA) = EMA(9) of MACD line
/// All values are based on confirmed (closed) candles only.
pub struct MacdCalculator {
    fast_ema: Ema,
    slow_ema: Ema,
    signal_ema: Ema,
    /// 最近3个 confirmed bar 值 (DIF - DEA)，用于判断柱状图趋势
    /// [0] = 前前bar, [1] = 前bar, [2] = 当前bar
    recent_bars: [f64; 3],
    bar_count: usize,
}

impl MacdCalculator {
    pub fn new() -> Self {
        Self {
            fast_ema: Ema::new(12),
            slow_ema: Ema::new(26),
            signal_ema: Ema::new(9),
            recent_bars: [0.0; 3],
            bar_count: 0,
        }
    }

    /// Commit a confirmed close price
    pub fn update(&mut self, close: f64) {
        self.fast_ema.update(close);
        self.slow_ema.update(close);
        if let (Some(fast), Some(slow)) = (self.fast_ema.value(), self.slow_ema.value()) {
            self.signal_ema.update(fast - slow);
            if let Some(dea) = self.signal_ema.value() {
                let bar = (fast - slow) - dea;
                self.recent_bars[0] = self.recent_bars[1];
                self.recent_bars[1] = self.recent_bars[2];
                self.recent_bars[2] = bar;
                self.bar_count += 1;
            }
        }
    }

    /// MACD line (DIF) = fast EMA - slow EMA
    pub fn macd_line(&self) -> Option<f64> {
        match (self.fast_ema.value(), self.slow_ema.value()) {
            (Some(fast), Some(slow)) => Some(fast - slow),
            _ => None,
        }
    }

    /// DEA (signal line) value
    pub fn dea(&self) -> Option<f64> {
        self.signal_ema.value()
    }

    /// MACD bar (柱状图) = DIF - DEA
    pub fn bar(&self) -> Option<f64> {
        match (self.macd_line(), self.dea()) {
            (Some(dif), Some(dea)) => Some(dif - dea),
            _ => None,
        }
    }

    /// 柱状图趋势：连续3根 confirmed bar 递增返回 1，递减返回 -1，否则 0
    pub fn bar_trend(&self) -> i8 {
        if self.bar_count < 3 {
            return 0;
        }
        let [a, b, c] = self.recent_bars;
        if c > b && b > a {
            1
        } else if c < b && b < a {
            -1
        } else {
            0
        }
    }

    /// Slow EMA (26-period) value, used as price reference for overbought/oversold
    pub fn slow_ema_value(&self) -> Option<f64> {
        self.slow_ema.value()
    }

    pub fn is_ready(&self) -> bool {
        self.signal_ema.is_ready()
    }
}

/// ATR Calculator (Wilder's smoothing, default period 14)
///
/// True Range = max(H-L, |H-prev_close|, |L-prev_close|)
/// First ATR = SMA of first N true ranges
/// Then: ATR = (prev_ATR * (N-1) + TR) / N
/// Only uses confirmed candles.
pub struct AtrCalculator {
    period: usize,
    committed: Option<f64>,
    count: usize,
    sum: f64,
    prev_close: Option<f64>,
}

impl AtrCalculator {
    pub fn new(period: usize) -> Self {
        Self {
            period,
            committed: None,
            count: 0,
            sum: 0.0,
            prev_close: None,
        }
    }

    /// Commit a confirmed candle, advancing internal state
    pub fn update(&mut self, high: f64, low: f64, close: f64) {
        let tr = self.true_range(high, low);
        self.prev_close = Some(close);

        self.count += 1;
        if self.count < self.period {
            self.sum += tr;
        } else if self.count == self.period {
            self.sum += tr;
            self.committed = Some(self.sum / self.period as f64);
        } else {
            let prev = self.committed.unwrap();
            self.committed = Some((prev * (self.period as f64 - 1.0) + tr) / self.period as f64);
        }
    }

    fn true_range(&self, high: f64, low: f64) -> f64 {
        if let Some(prev_close) = self.prev_close {
            (high - low)
                .max((high - prev_close).abs())
                .max((low - prev_close).abs())
        } else {
            high - low
        }
    }

    pub fn value(&self) -> Option<f64> {
        self.committed
    }

    pub fn is_ready(&self) -> bool {
        self.committed.is_some()
    }
}
