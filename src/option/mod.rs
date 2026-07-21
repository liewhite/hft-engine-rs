//! 期权定价 (Black-Scholes)。回测的合成希腊字母源 ([`crate::backtest::BsGreeksSource`]) 据此估值。

mod black_scholes;

pub use black_scholes::{greeks, norm_cdf, norm_pdf, BsGreeks, OptionRight, MILLIS_PER_YEAR};
