/// Exponential Moving Average (incremental)
///
/// Warm-up: first `period` values use SMA, then switch to EMA
pub struct Ema {
    period: usize,
    multiplier: f64,
    value: Option<f64>,
    count: usize,
    sum: f64,
}

impl Ema {
    pub fn new(period: usize) -> Self {
        Self {
            period,
            multiplier: 2.0 / (period as f64 + 1.0),
            value: None,
            count: 0,
            sum: 0.0,
        }
    }

    pub fn update(&mut self, value: f64) {
        self.count += 1;
        if self.count < self.period {
            self.sum += value;
        } else if self.count == self.period {
            self.sum += value;
            self.value = Some(self.sum / self.period as f64);
        } else {
            let prev = self.value.unwrap();
            self.value = Some(prev + self.multiplier * (value - prev));
        }
    }

    pub fn value(&self) -> Option<f64> {
        self.value
    }

    pub fn is_ready(&self) -> bool {
        self.value.is_some()
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

    pub fn update(&mut self, close: f64) {
        self.fast_ema.update(close);
        self.slow_ema.update(close);
        if let (Some(fast), Some(slow)) = (self.fast_ema.value(), self.slow_ema.value()) {
            self.signal_ema.update(fast - slow);
        }
    }

    /// DEA (signal line) value
    pub fn dea(&self) -> Option<f64> {
        self.signal_ema.value()
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
    value: Option<f64>,
    count: usize,
    sum: f64,
    prev_close: Option<f64>,
}

impl AtrCalculator {
    pub fn new(period: usize) -> Self {
        Self {
            period,
            value: None,
            count: 0,
            sum: 0.0,
            prev_close: None,
        }
    }

    pub fn update(&mut self, high: f64, low: f64, close: f64) {
        let tr = if let Some(prev_close) = self.prev_close {
            (high - low)
                .max((high - prev_close).abs())
                .max((low - prev_close).abs())
        } else {
            high - low
        };
        self.prev_close = Some(close);

        self.count += 1;
        if self.count < self.period {
            self.sum += tr;
        } else if self.count == self.period {
            self.sum += tr;
            self.value = Some(self.sum / self.period as f64);
        } else {
            let prev = self.value.unwrap();
            self.value = Some((prev * (self.period as f64 - 1.0) + tr) / self.period as f64);
        }
    }

    pub fn value(&self) -> Option<f64> {
        self.value
    }

    pub fn is_ready(&self) -> bool {
        self.value.is_some()
    }
}
