//! IBKR 线路格式的取值工具：**同一字段可能是 Number 也可能是 String**。
//!
//! IBKR CPAPI 对同一语义的字段在不同接口/不同时刻会用不同 JSON 类型下发，数字还可能
//! 带千分位逗号（`"1,234.5"`）。这不是可以各处随手兜一下的小事：
//!
//! - 只按 `as_u64()` 解析 `orderId`，字符串形态会得到空 id —— 撤单必失败，撤单复查还会
//!   因空 id 匹配不到挂单而把**活单误判成已终态**；
//! - 只按 `as_f64()` 解析价量，字符串形态得到 `None`，若再兜 0 就是把"没解析出来"说成
//!   "值是 0"，直接污染账本。
//!
//! 所以解析口径必须**只有一处**。此前 orderId 的双态解析在三个地方各抄了一份
//! （两条 WS 路径 + REST），行为还不一致，正是"改一处忘两处"的温床。

/// 把一个 JSON 值取成数字：接受 Number 与 String（含千分位逗号）两种形态。
///
/// 返回 `None` 表示**这个值不是数字**（而非"字段不存在"，那由调用方区分，
/// 见 [`optional_number`]）。
pub(crate) fn number(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => {
            let cleaned = s.replace(',', "");
            match cleaned.parse::<f64>() {
                Ok(n) => Some(n),
                Err(_) => {
                    tracing::warn!(raw = %s, "Failed to parse IB number");
                    None
                }
            }
        }
        _ => None,
    }
}

/// 三态取数：字段缺失/为 null → `Ok(None)`；字段在但不是数字 → `Err`；有效 → `Ok(Some(v))`。
///
/// "没有这个字段"与"值是 0"是两件事：前者常见且合法（IBKR 第二条成交消息通常只补
/// commission、不重复带 price/size），后者若来自解析失败会直接污染账本。**解析失败绝不兜 0**。
pub(crate) fn optional_number(
    item: &serde_json::Value,
    field: &str,
) -> Result<Option<f64>, String> {
    match item.get(field) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(v) => number(v)
            .map(Some)
            .ok_or_else(|| format!("IBKR 字段 {field} 无法解析为数字: {v}")),
    }
}

/// 取订单号：Number 与 String 两种形态都接。
///
/// 返回 `None` = 该字段缺失、为 null、或类型压根不是标量 —— 一律按"**没有订单号**"处理，
/// 绝不编一个字符串（如把 `Null` 变成 `"null"`）顶上：下游的撤单复查对空 id 有专门的
/// 保守分支（`Unverifiable`，宁可留着也不误清活单），编造值会绕过它。
pub(crate) fn order_id(v: Option<&serde_json::Value>) -> Option<String> {
    match v? {
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 两种形态都要接，且千分位逗号要能处理
    #[test]
    fn numbers_accept_both_wire_forms() {
        assert_eq!(number(&json!(1234.5)), Some(1234.5));
        assert_eq!(number(&json!("1,234.5")), Some(1234.5));
        assert_eq!(number(&json!("n/a")), None);
        assert_eq!(number(&json!(null)), None);
    }

    /// **三态**：缺失是合法的 None，字段在但坏是 Err —— 绝不兜 0
    #[test]
    fn optional_number_separates_missing_from_unparsable() {
        let item = json!({"price": "1,234.5", "size": null, "commission": "n/a"});
        assert_eq!(optional_number(&item, "price").unwrap(), Some(1234.5));
        assert_eq!(optional_number(&item, "size").unwrap(), None, "null 视为缺失");
        assert_eq!(optional_number(&item, "absent").unwrap(), None, "缺失就是 None");
        assert!(
            optional_number(&item, "commission").is_err(),
            "字段在但解析不了必须报错，兜 0 会把手续费/价量静默写错"
        );
    }

    /// **Critical 回归防线**：orderId 两种形态都要认，且绝不编造。
    ///
    /// 只按 u64 解析会让字符串形态得到空 id（撤单失败、复查把活单误判成已终态）；
    /// 而把 `null` 变成 `"null"` 是另一个方向的错 —— 它会绕过复查对空 id 的保守分支。
    #[test]
    fn order_id_accepts_both_forms_and_never_fabricates() {
        assert_eq!(order_id(Some(&json!(778901234))), Some("778901234".to_string()));
        assert_eq!(order_id(Some(&json!("778901234"))), Some("778901234".to_string()));
        assert_eq!(order_id(None), None, "字段缺失就是没有订单号");
        assert_eq!(order_id(Some(&json!(null))), None, "null 不能变成字符串 \"null\"");
        assert_eq!(order_id(Some(&json!(""))), None, "空串等于没有");
        assert_eq!(order_id(Some(&json!({"a": 1}))), None, "非标量不能 to_string 顶上");
    }
}
