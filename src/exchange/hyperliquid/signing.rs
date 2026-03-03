//! Hyperliquid EIP-712 签名实现
//!
//! 参考: https://github.com/hyperliquid-dex/hyperliquid-rust-sdk

use alloy::primitives::{keccak256, Address, FixedBytes, B256};
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::Signer;
use alloy::sol;
use alloy::sol_types::SolStruct;
use serde::{Deserialize, Serialize};

/// EIP-712 Domain 常量
const DOMAIN_NAME: &str = "Exchange";
const DOMAIN_VERSION: &str = "1";
const DOMAIN_CHAIN_ID: u64 = 1337;

/// Phantom Agent source
const SOURCE_MAINNET: &str = "a";
const SOURCE_TESTNET: &str = "b";

// 定义 EIP-712 类型
sol! {
    #[derive(Debug)]
    struct Agent {
        string source;
        bytes32 connectionId;
    }
}

/// L1 Action 签名结果
#[derive(Debug, Clone)]
pub struct L1Signature {
    pub r: B256,
    pub s: B256,
    pub v: u8,
}

impl L1Signature {
    /// 转换为 Hyperliquid API 格式
    pub fn to_api_format(&self) -> SignatureJson {
        SignatureJson {
            r: format!("0x{}", hex::encode(self.r)),
            s: format!("0x{}", hex::encode(self.s)),
            v: self.v,
        }
    }
}

/// API 签名格式
#[derive(Debug, Clone, Serialize)]
pub struct SignatureJson {
    pub r: String,
    pub s: String,
    pub v: u8,
}

/// 计算 action hash
///
/// 算法:
/// 1. 用 msgpack 序列化 action
/// 2. 附加 nonce 的大端字节
/// 3. 如果有 vault_address，附加 1 + address bytes
/// 4. Keccak256 哈希
pub fn action_hash<T: Serialize>(
    action: &T,
    nonce: u64,
    vault_address: Option<&str>,
) -> Result<B256, String> {
    let mut data = rmp_serde::to_vec_named(action)
        .map_err(|e| format!("Failed to serialize action: {}", e))?;

    // 附加 nonce (大端)
    data.extend_from_slice(&nonce.to_be_bytes());

    // 如果有 vault_address
    if let Some(addr) = vault_address {
        data.push(1u8);
        let addr_bytes = hex::decode(addr.trim_start_matches("0x"))
            .map_err(|e| format!("Invalid vault address: {}", e))?;
        data.extend_from_slice(&addr_bytes);
    } else {
        data.push(0u8);
    }

    Ok(keccak256(&data))
}

/// 签名 L1 action
pub async fn sign_l1_action(
    signer: &PrivateKeySigner,
    connection_id: B256,
    is_mainnet: bool,
) -> Result<L1Signature, String> {
    let source = if is_mainnet { SOURCE_MAINNET } else { SOURCE_TESTNET };

    // 构造 phantom agent
    let agent = Agent {
        source: source.to_string(),
        connectionId: FixedBytes::from_slice(connection_id.as_slice()),
    };

    // 构造 EIP-712 domain
    let domain = alloy::sol_types::eip712_domain! {
        name: DOMAIN_NAME,
        version: DOMAIN_VERSION,
        chain_id: DOMAIN_CHAIN_ID,
        verifying_contract: Address::ZERO,
    };

    // 计算 signing hash
    let signing_hash = agent.eip712_signing_hash(&domain);

    // 签名
    let signature = signer
        .sign_hash(&signing_hash)
        .await
        .map_err(|e| format!("Failed to sign: {}", e))?;

    Ok(L1Signature {
        r: B256::from_slice(&signature.r().to_be_bytes::<32>()),
        s: B256::from_slice(&signature.s().to_be_bytes::<32>()),
        v: signature.v() as u8 + 27,
    })
}

/// 从私钥创建 signer
pub fn create_signer(private_key: &str) -> Result<PrivateKeySigner, String> {
    let key = private_key.trim_start_matches("0x");
    let bytes = hex::decode(key).map_err(|e| format!("Invalid private key hex: {}", e))?;

    PrivateKeySigner::from_slice(&bytes)
        .map_err(|e| format!("Invalid private key: {}", e))
}

// ============================================================================
// Order Action 结构
// ============================================================================

/// 订单请求 (wire format)
/// 字段使用单字母以减少 msgpack 序列化大小
#[derive(Debug, Clone, Serialize)]
pub struct OrderWire {
    /// Asset index
    #[serde(rename = "a")]
    pub asset: u32,
    /// Is buy
    #[serde(rename = "b")]
    pub is_buy: bool,
    /// Limit price (string to preserve precision)
    #[serde(rename = "p")]
    pub limit_px: String,
    /// Size (string to preserve precision)
    #[serde(rename = "s")]
    pub sz: String,
    /// Reduce only
    #[serde(rename = "r")]
    pub reduce_only: bool,
    /// Order type
    #[serde(rename = "t")]
    pub order_type: OrderType,
    /// Client order ID (optional)
    #[serde(rename = "c", skip_serializing_if = "Option::is_none")]
    pub cloid: Option<String>,
}

/// 订单类型
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum OrderType {
    Limit(LimitOrder),
    // Trigger(TriggerOrder),
}

/// 限价单参数
#[derive(Debug, Clone, Serialize)]
pub struct LimitOrder {
    pub tif: String,
}

/// 触发单参数
// #[derive(Debug, Clone, Serialize)]
// #[serde(rename_all = "camelCase")]
// pub struct TriggerOrder {
//     pub is_market: bool,
//     pub trigger_px: String,
//     pub tpsl: String,
// }

/// 批量下单 Action
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BulkOrderAction {
    #[serde(rename = "type")]
    pub action_type: String,
    pub orders: Vec<OrderWire>,
    pub grouping: String,
}

impl BulkOrderAction {
    pub fn new(orders: Vec<OrderWire>) -> Self {
        Self {
            action_type: "order".to_string(),
            orders,
            grouping: "na".to_string(),
        }
    }
}

/// Exchange API 请求体
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExchangeRequest {
    pub action: serde_json::Value,
    pub nonce: u64,
    pub signature: SignatureJson,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vault_address: Option<String>,
}

/// 下单响应
///
/// 成功时: `{"status":"ok","response":{"type":"order","data":{...}}}`
/// 失败时: `{"status":"err","response":"error message"}`
#[derive(Debug, Clone, Deserialize)]
pub struct OrderResponse {
    pub status: String,
    /// 成功时为 OrderResponseData 对象，失败时为错误消息字符串
    pub response: Option<serde_json::Value>,
}

/// 下单响应数据
#[derive(Debug, Clone, Deserialize)]
pub struct OrderResponseData {
    // #[serde(rename = "type")]
    // pub response_type: String,
    pub data: Option<OrderResponseDataInner>,
}

/// 下单响应内部数据
#[derive(Debug, Clone, Deserialize)]
pub struct OrderResponseDataInner {
    pub statuses: Vec<OrderStatus>,
}

/// 单个订单状态
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum OrderStatus {
    Resting(OrderResting),
    Filled(OrderFilled),
    Error(OrderError),
}

#[derive(Debug, Clone, Deserialize)]
pub struct OrderResting {
    pub resting: OrderRestingData,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OrderRestingData {
    pub oid: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OrderFilled {
    pub filled: OrderFilledData,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderFilledData {
    // pub total_sz: String,
    // pub avg_px: String,
    pub oid: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OrderError {
    pub error: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_action_hash() {
        // 简单测试 action hash 计算
        #[derive(Serialize)]
        struct TestAction {
            #[serde(rename = "type")]
            action_type: String,
        }

        let action = TestAction {
            action_type: "test".to_string(),
        };

        let hash = action_hash(&action, 1234567890000, None).expect("hash should succeed");
        assert_eq!(hash.len(), 32);
    }
}
