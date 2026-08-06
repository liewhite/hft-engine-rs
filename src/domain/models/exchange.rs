use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

/// 交易所枚举
///
/// `Ord` 按声明顺序排序，仅用于让"交易所集合"有确定的展示/遍历顺序（日志与启动可复现），
/// 不含业务优先级语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Exchange {
    Binance,
    OKX,
    Hyperliquid,
    IBKR,
}

impl Exchange {
    /// 本引擎下单时给 client_order_id 加的前缀。
    ///
    /// 用途不止是满足各所的格式要求，更是**归属标记**：同一个交易所账户上可能还有人工下单
    /// 或其他程序下的单，凭前缀才能把"本引擎的单"识别出来（见
    /// [`Self::owns_cli_order_id`]）——启动期撤掉遗留挂单时，撤错别人的单是不可接受的。
    ///
    /// - Binance/Hyperliquid: `0x`（Hyperliquid 的 `cloid` 要求 128-bit 十六进制，即
    ///   `0x` + 32 hex，故此格式是刻意为之，不可随意改）
    /// - OKX: `x`（OKX 不允许以数字 `0` 开头）
    /// - IBKR: `ib`
    pub fn cli_order_id_prefix(&self) -> &'static str {
        match self {
            Exchange::OKX => "x",
            Exchange::IBKR => "ib",
            Exchange::Binance | Exchange::Hyperliquid => "0x",
        }
    }

    /// 生成交易所特定的 client_order_id（长度受各所上限约束）
    pub fn new_cli_order_id(&self) -> String {
        let uuid_hex = Uuid::new_v4().simple().to_string();
        match self {
            Exchange::OKX => format!("x{}", &uuid_hex[..31]),
            Exchange::IBKR => format!("ib{}", &uuid_hex[..30]),
            // 保持 `0x` + 完整 32 hex：Hyperliquid 的 cloid 要求正是这个长度
            Exchange::Binance | Exchange::Hyperliquid => format!("0x{}", uuid_hex),
        }
    }

    /// 该 client_order_id 是否由本引擎在此交易所生成。
    ///
    /// 用于把本引擎的单与同账户上人工 / 其他程序下的单区分开。判据只看前缀，因此
    /// [`Self::cli_order_id_prefix`] 与 [`Self::new_cli_order_id`] 必须始终一致 ——
    /// 由 `prefix_matches_generated_id` 测试守住。
    ///
    /// # 各所的判别力并不相同
    ///
    /// - **OKX / IBKR / Binance**：前缀是**我们自己选的**，外部单几乎不会撞上，判别力强。
    /// - **Hyperliquid**：协议规定 `cloid` 必须是 128-bit 十六进制（即 `0x` + 32 hex），
    ///   所以 `0x` 前缀是**格式要求而非我们的标记**，本身不携带归属信息。HL 上真正有区分力
    ///   的信号是"**有没有 cloid**"——人工经 UI 下单、以及强平等系统单都是 `cloid: null`
    ///   （2026-08 实测某账户 1987 张历史单中 331 张为 null）。故调用方对 HL 必须先要求
    ///   `client_order_id` 存在，本方法只作格式兜底。
    ///
    /// 无论哪个所，前缀都**认不出本引擎的另一个实例**（前缀相同）。分桶部署下这是可接受的：
    /// 各实例负责的 symbol 不重叠，而调用方（启动期撤单）只处理本实例负责的 symbol。
    pub fn owns_cli_order_id(&self, client_order_id: &str) -> bool {
        !client_order_id.is_empty() && client_order_id.starts_with(self.cli_order_id_prefix())
    }
}

/// 全部交易所，供需要逐所遍历的测试与工具使用（新增交易所时编译器会在此处提醒补齐）。
/// 当前只有测试在用，故显式 allow —— 它的价值是"新增所时有一处会编译失败"，不是被谁调用。
#[allow(dead_code)]
pub const ALL_EXCHANGES: [Exchange; 4] = [
    Exchange::Binance,
    Exchange::OKX,
    Exchange::Hyperliquid,
    Exchange::IBKR,
];

impl fmt::Display for Exchange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Exchange::Binance => write!(f, "Binance"),
            Exchange::OKX => write!(f, "OKX"),
            Exchange::Hyperliquid => write!(f, "Hyperliquid"),
            Exchange::IBKR => write!(f, "IBKR"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 前缀与实际生成的 id 必须一致 —— 否则 `owns_cli_order_id` 会把自己的单判成别人的，
    /// 启动期就撤不掉遗留挂单（策略随后会在旁边重复挂单）。
    #[test]
    fn prefix_matches_generated_id() {
        for exchange in ALL_EXCHANGES {
            let id = exchange.new_cli_order_id();
            assert!(
                id.starts_with(exchange.cli_order_id_prefix()),
                "{exchange}: 生成的 id {id} 不以声明的前缀 {} 开头",
                exchange.cli_order_id_prefix()
            );
            assert!(exchange.owns_cli_order_id(&id), "{exchange}: 认不出自己生成的 id");
        }
    }

    /// 各所前缀互不误判：一个所生成的 id 不该被另一个所认领。
    ///
    /// Binance/Hyperliquid 共用 `0x`（两者的账户不会混在一起，前缀相同无害），
    /// 但 `0x` 与 OKX 的 `x`、IBKR 的 `ib` 之间必须互斥。
    #[test]
    fn prefixes_do_not_cross_claim() {
        let okx_id = Exchange::OKX.new_cli_order_id();
        let ibkr_id = Exchange::IBKR.new_cli_order_id();
        let binance_id = Exchange::Binance.new_cli_order_id();

        assert!(!Exchange::Binance.owns_cli_order_id(&okx_id));
        assert!(!Exchange::Binance.owns_cli_order_id(&ibkr_id));
        assert!(!Exchange::OKX.owns_cli_order_id(&binance_id));
        assert!(!Exchange::OKX.owns_cli_order_id(&ibkr_id));
        assert!(!Exchange::IBKR.owns_cli_order_id(&binance_id));
        assert!(!Exchange::IBKR.owns_cli_order_id(&okx_id));
    }

    /// 外部下单（人工 / 其他程序）不该被认领
    #[test]
    fn foreign_client_order_ids_are_not_owned() {
        // Binance 网页/App 下单的典型 client_order_id 形态
        for foreign in ["web_1a2b3c", "android_9f8e", "ios_x1", "", "electron_7"] {
            for exchange in ALL_EXCHANGES {
                assert!(
                    !exchange.owns_cli_order_id(foreign),
                    "{exchange} 误认领了外部订单 id {foreign:?}"
                );
            }
        }
    }
}
