/// Exponential Moving Average (incremental)
///
/// Warm-up: first `period` values use SMA, then switch to EMA.
/// Supports live preview: `update_live()` computes a temporary value from committed state
/// without advancing it. Multiple live calls overwrite each other (replace, not append).
/// `update()` commits and clears the live value.
pub struct Ema {
    period: usize,
    multiplier: f64,
    /// Committed value (after last confirmed data point)
    committed: Option<f64>,
    /// Live value (including current unconfirmed data point)
    live: Option<f64>,
    count: usize,
    sum: f64,
}

impl Ema {
    pub fn new(period: usize) -> Self {
        Self {
            period,
            multiplier: 2.0 / (period as f64 + 1.0),
            committed: None,
            live: None,
            count: 0,
            sum: 0.0,
        }
    }

    /// Commit a confirmed data point, advancing internal state
    pub fn update(&mut self, value: f64) {
        self.live = None;
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

    /// Preview an unconfirmed data point without advancing state.
    /// Computes from committed state, so multiple calls for the same bar correctly replace.
    pub fn update_live(&mut self, value: f64) {
        if self.count < self.period - 1 {
            // Even with one more point, not enough for SMA
            self.live = None;
        } else if self.count == self.period - 1 {
            // One more point completes the SMA warm-up
            self.live = Some((self.sum + value) / self.period as f64);
        } else {
            // EMA from committed value
            let prev = self.committed.unwrap();
            self.live = Some(prev + self.multiplier * (value - prev));
        }
    }

    /// Returns live value if present, otherwise committed value
    pub fn value(&self) -> Option<f64> {
        self.live.or(self.committed)
    }

    pub fn is_ready(&self) -> bool {
        self.live.is_some() || self.committed.is_some()
    }
}

/// MACD Calculator (standard 12/26/9)
///
/// - MACD line = fast EMA(12) - slow EMA(26)
/// - Signal line (DEA) = EMA(9) of MACD line
pub struct MacdCalculator {
    fast_ema: Ema,
    slow_ema: Ema,
    signal_ema: Ema,
}

impl MacdCalculator {
    pub fn new() -> Self {
        Self {
            fast_ema: Ema::new(12),
            slow_ema: Ema::new(26),
            signal_ema: Ema::new(9),
        }
    }

    /// Commit a confirmed close price
    pub fn update(&mut self, close: f64) {
        self.fast_ema.update(close);
        self.slow_ema.update(close);
        if let (Some(fast), Some(slow)) = (self.fast_ema.value(), self.slow_ema.value()) {
            self.signal_ema.update(fast - slow);
        }
    }

    /// Preview an unconfirmed close price without advancing state
    pub fn update_live(&mut self, close: f64) {
        self.fast_ema.update_live(close);
        self.slow_ema.update_live(close);
        if let (Some(fast), Some(slow)) = (self.fast_ema.value(), self.slow_ema.value()) {
            self.signal_ema.update_live(fast - slow);
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
pub struct AtrCalculator {
    period: usize,
    committed: Option<f64>,
    live: Option<f64>,
    count: usize,
    sum: f64,
    prev_close: Option<f64>,
}

impl AtrCalculator {
    pub fn new(period: usize) -> Self {
        Self {
            period,
            committed: None,
            live: None,
            count: 0,
            sum: 0.0,
            prev_close: None,
        }
    }

    /// Commit a confirmed candle, advancing internal state
    pub fn update(&mut self, high: f64, low: f64, close: f64) {
        self.live = None;
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

    /// Preview an unconfirmed candle without advancing state.
    /// Uses committed prev_close for TR calculation, does not update prev_close.
    pub fn update_live(&mut self, high: f64, low: f64, _close: f64) {
        let tr = self.true_range(high, low);

        if self.count < self.period - 1 {
            self.live = None;
        } else if self.count == self.period - 1 {
            self.live = Some((self.sum + tr) / self.period as f64);
        } else {
            let prev = self.committed.unwrap();
            self.live = Some((prev * (self.period as f64 - 1.0) + tr) / self.period as f64);
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

    /// Returns live value if present, otherwise committed value
    pub fn value(&self) -> Option<f64> {
        self.live.or(self.committed)
    }

    pub fn is_ready(&self) -> bool {
        self.live.is_some() || self.committed.is_some()
    }
}
