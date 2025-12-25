use serde::{Deserialize, Serialize};
use std::fmt;

/// 交易方向
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    Long,
    Short,
}

impl Side {
    pub fn opposite(&self) -> Self {
        match self {
            Side::Long => Side::Short,
            Side::Short => Side::Long,
        }
    }
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Side::Long => write!(f, "Long"),
            Side::Short => write!(f, "Short"),
        }
    }
}
