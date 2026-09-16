//! Session value addresses and write builders — port of
//! `packages/agent/src/harness/session/values.ts` (0.85.1).
//!
//! Values and lists are addressed by `(namespace, key)`; writes are plain
//! data (`set`/`delete`/`append`) executed by the session backend. Address
//! validation mirrors upstream: empty namespaces and NUL bytes are
//! rejected.

/// Address of a stored value or list.
#[derive(Debug, Clone, PartialEq)]
pub struct ValueAddress {
    pub namespace: String,
    pub key: String,
    pub kind: AddressKind,
}

/// Whether an address names a value or a list.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AddressKind {
    Value,
    List,
}

/// A value write: `set` or `delete`.
#[derive(Debug, Clone, PartialEq)]
pub struct ValueWrite {
    pub kind: &'static str,
    pub op: String,
    pub namespace: String,
    pub key: String,
    pub value: Option<serde_json::Value>,
}

/// A list write: `append` or `delete`.
#[derive(Debug, Clone, PartialEq)]
pub struct ListWrite {
    pub kind: &'static str,
    pub op: String,
    pub namespace: String,
    pub key: String,
    pub value: Option<serde_json::Value>,
}

fn validate_address(namespace: &str, key: &str) -> Result<(), String> {
    if namespace.is_empty() {
        return Err("Value namespace must not be empty".to_string());
    }
    if namespace.contains('\0') {
        return Err("Value namespace must not contain \\u0000".to_string());
    }
    if key.contains('\0') {
        return Err("Value key must not contain \\u0000".to_string());
    }
    Ok(())
}

/// Build a value address (upstream `value(namespace, key)`; upstream
/// throws `TypeError` on invalid addresses, mirrored here as `Err`).
pub fn value(namespace: &str, key: &str) -> Result<ValueAddress, String> {
    validate_address(namespace, key)?;
    Ok(ValueAddress {
        namespace: namespace.to_string(),
        key: key.to_string(),
        kind: AddressKind::Value,
    })
}

/// Build a list address (upstream `list(namespace, key)`).
pub fn list(namespace: &str, key: &str) -> Result<ValueAddress, String> {
    validate_address(namespace, key)?;
    Ok(ValueAddress {
        namespace: namespace.to_string(),
        key: key.to_string(),
        kind: AddressKind::List,
    })
}

/// Build a value-set write (upstream `setValue`).
pub fn set_value(address: &ValueAddress, next: serde_json::Value) -> ValueWrite {
    ValueWrite {
        kind: "value",
        op: "set".to_string(),
        namespace: address.namespace.clone(),
        key: address.key.clone(),
        value: Some(next),
    }
}

/// Build a value-delete write (upstream `deleteValue`).
pub fn delete_value(address: &ValueAddress) -> ValueWrite {
    ValueWrite {
        kind: "value",
        op: "delete".to_string(),
        namespace: address.namespace.clone(),
        key: address.key.clone(),
        value: None,
    }
}

/// Build a list-append write (upstream `appendList`).
pub fn append_list(address: &ValueAddress, element: serde_json::Value) -> ListWrite {
    ListWrite {
        kind: "list",
        op: "append".to_string(),
        namespace: address.namespace.clone(),
        key: address.key.clone(),
        value: Some(element),
    }
}

/// Build a list-delete write (upstream `deleteList`).
pub fn delete_list(address: &ValueAddress) -> ListWrite {
    ListWrite {
        kind: "list",
        op: "delete".to_string(),
        namespace: address.namespace.clone(),
        key: address.key.clone(),
        value: None,
    }
}

/// List read options (upstream `ListReadOptions`).
#[derive(Debug, Clone, Default)]
pub struct ListReadOptions {
    pub cursor: Option<u64>,
    pub order: Option<String>,
    pub limit: Option<u64>,
}

/// Resolved list read options (upstream `resolveListReadOptions`): default
/// limit 1000, cap 10000, `asc` default order; non-positive limits rejected.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedListReadOptions {
    pub cursor: Option<u64>,
    pub order: String,
    pub limit: u64,
}

/// Resolve list read options, rejecting non-positive limits (upstream
/// `resolveListReadOptions`).
pub fn resolve_list_read_options(
    options: ListReadOptions,
) -> Result<ResolvedListReadOptions, String> {
    let requested = options.limit.unwrap_or(1_000);
    if requested == 0 {
        return Err("List read limit must be a positive safe integer".to_string());
    }
    Ok(ResolvedListReadOptions {
        cursor: options.cursor,
        order: options.order.unwrap_or_else(|| "asc".to_string()),
        limit: requested.min(10_000),
    })
}

/// Well-known value addresses (upstream `values.ts` exports).
pub fn branch_tip(branch: &str) -> Result<ValueAddress, String> {
    value("pi.branch.tip", branch)
}

pub fn session_name() -> Result<ValueAddress, String> {
    value("pi.session.name", "")
}

pub fn entry_label(entry_id: &str) -> Result<ValueAddress, String> {
    value("pi.entry.label", entry_id)
}

pub fn operation_tool_memo(
    operation_id: &str,
    invocation_id: &str,
    name: &str,
) -> Result<ValueAddress, String> {
    value(
        "pi.op.tool_memo",
        &format!("{operation_id}:{invocation_id}:{name}"),
    )
}
