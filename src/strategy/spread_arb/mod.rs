mod config;
mod ema;
mod signals;
mod strategy;

pub use config::SpreadArbConfig;
pub use strategy::{SpreadArbStrategy, MIN_EXCHANGES_PER_SYMBOL};
