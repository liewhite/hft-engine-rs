/// EMA (Exponential Moving Average) 计算器
#[derive(Debug, Clone)]
pub struct EmaCalculator {
    period: usize,
    alpha: f64,
    value: Option<f64>,
    count: usize,
}

impl EmaCalculator {
    pub fn new(period: usize) -> Self {
        let alpha = 2.0 / (period as f64 + 1.0);
        Self {
            period,
            alpha,
            value: None,
            count: 0,
        }
    }

    /// 更新 EMA，返回当前 EMA 值
    pub fn update(&mut self, new_value: f64) -> f64 {
        self.count += 1;
        match self.value {
            None => {
                self.value = Some(new_value);
                new_value
            }
            Some(prev) => {
                let ema = self.alpha * new_value + (1.0 - self.alpha) * prev;
                self.value = Some(ema);
                ema
            }
        }
    }

    /// 获取当前 EMA 值
    pub fn value(&self) -> Option<f64> {
        self.value
    }

    /// 是否已经预热完成（满足 period 次更新）
    pub fn is_ready(&self) -> bool {
        self.count >= self.period
    }
}

/// 交易所的 bid/ask EMA
#[derive(Debug, Clone)]
pub struct ExchangeEma {
    pub bid_ema: EmaCalculator,
    pub ask_ema: EmaCalculator,
}

impl ExchangeEma {
    pub fn new(period: usize) -> Self {
        Self {
            bid_ema: EmaCalculator::new(period),
            ask_ema: EmaCalculator::new(period),
        }
    }

    pub fn is_ready(&self) -> bool {
        self.bid_ema.is_ready() && self.ask_ema.is_ready()
    }
}
