//! Tool wrappers for extension-registered tools — port of
//! `packages/coding-agent/src/core/extensions/wrapper.ts`.
//!
//! The upstream wrapper adapts a `RegisteredTool` into an `AgentTool` that
//! (a) uses the runner's `createContext()` for consistent context and
//! (b) after execution, merges tool names that the execution added via
//! `pi.setActiveTools()` into the result's `addedToolNames`.
//!
//! The Rust port models the same wrapper around a generic tool-execution
//! closure so the `addedToolNames` merge contract is testable without a live
//! agent loop.

use crate::core::extensions::types::RegisteredTool;

/// Tool-execution input for the wrapped call. The active-tool set around the
/// execution is captured by the caller (the runner in the JS port).
#[derive(Debug, Clone)]
pub struct WrappedToolCall {
    pub tool_call_id: String,
    pub params: serde_json::Value,
    pub active_tools_before: Vec<String>,
    pub active_tools_after: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct WrappedToolResult {
    pub content: Vec<serde_json::Value>,
    pub is_error: bool,
    pub added_tool_names: Vec<String>,
}

impl WrappedToolResult {
    /// Merge additional added-tool names, deduplicating while preserving
    /// order (upstream `new Set([...])`).
    pub fn merge_added_tool_names(mut self, names: Vec<String>) -> Self {
        for name in names {
            if !self.added_tool_names.contains(&name) {
                self.added_tool_names.push(name);
            }
        }
        self
    }
}

/// The port's AgentTool-shaped surface for extension tool wrapping.
#[derive(Clone)]
pub struct WrappedTool {
    pub definition: RegisteredTool,
    pub execute: std::sync::Arc<dyn Fn(WrappedToolCall) -> WrappedToolResult + Send + Sync>,
}

/// Wrap a `RegisteredTool` into a `WrappedTool` (upstream `wrapRegisteredTool`).
///
/// The wrapped execute closure runs the underlying tool execution, then
/// computes `addedToolNames` as the difference between the active-tool set
/// after execution and before it, merging that diff into the result.
pub fn wrap_registered_tool(
    registered_tool: RegisteredTool,
    execute: std::sync::Arc<dyn Fn(&WrappedToolCall) -> WrappedToolResult + Send + Sync>,
) -> WrappedTool {
    let definition = registered_tool.clone();
    let execute_inner = execute;
    let wrapped_execute: std::sync::Arc<dyn Fn(WrappedToolCall) -> WrappedToolResult + Send + Sync> =
        std::sync::Arc::new(move |call: WrappedToolCall| {
            let active_before: Vec<String> = call.active_tools_before.clone();
            let result = execute_inner(&call);
            let before_set: std::collections::BTreeSet<&String> = active_before.iter().collect();
            let added: Vec<String> = call
                .active_tools_after
                .iter()
                .filter(|name| !before_set.contains(name))
                .cloned()
                .collect();
            if added.is_empty() {
                return result;
            }
            result.merge_added_tool_names(added)
        });
    WrappedTool { definition, execute: wrapped_execute }
}

/// Wrap all registered tools (upstream `wrapRegisteredTools`).
pub fn wrap_registered_tools(
    registered_tools: Vec<RegisteredTool>,
    execute: std::sync::Arc<dyn Fn(&WrappedToolCall) -> WrappedToolResult + Send + Sync>,
) -> Vec<WrappedTool> {
    registered_tools
        .into_iter()
        .map(|tool| wrap_registered_tool(tool, execute.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::extensions::types::SourceInfo;

    fn tool(name: &str) -> RegisteredTool {
        RegisteredTool {
            name: name.to_string(),
            description: format!("{name} tool"),
            parameters: serde_json::Value::Object(Default::default()),
            source_info: SourceInfo::synthetic("ext", "local", None),
        }
    }

    fn simple_execute() -> std::sync::Arc<dyn Fn(&WrappedToolCall) -> WrappedToolResult + Send + Sync> {
        std::sync::Arc::new(|_call: &WrappedToolCall| WrappedToolResult::default())
    }

    #[test]
    fn wrapper_merges_added_tool_names_from_active_set_diff() {
        let wrapped = wrap_registered_tool(tool("ext-tool"), simple_execute());
        let call = WrappedToolCall {
            tool_call_id: "call-1".into(),
            params: serde_json::json!({}),
            active_tools_before: vec!["bash".into(), "read".into()],
            active_tools_after: vec!["bash".into(), "read".into(), "new-tool".into()],
        };
        let result = (wrapped.execute)(call);
        assert_eq!(result.added_tool_names, vec!["new-tool"]);
    }

    #[test]
    fn wrapper_deduplicates_added_names() {
        let result = WrappedToolResult { content: vec![], is_error: false, added_tool_names: vec!["x".into()] }
            .merge_added_tool_names(vec!["x".into(), "y".into()]);
        assert_eq!(result.added_tool_names, vec!["x", "y"]);
    }

    #[test]
    fn wrapper_without_added_names_returns_unchanged() {
        let wrapped = wrap_registered_tool(tool("ext-tool"), simple_execute());
        let call = WrappedToolCall {
            tool_call_id: "call-1".into(),
            params: serde_json::json!({}),
            active_tools_before: vec!["bash".into()],
            active_tools_after: vec!["bash".into()],
        };
        let result = (wrapped.execute)(call);
        assert!(result.added_tool_names.is_empty());
    }

    #[test]
    fn wrap_many_registers_all() {
        let tools = vec![tool("a"), tool("b")];
        let wrapped = wrap_registered_tools(tools, simple_execute());
        assert_eq!(wrapped.len(), 2);
        assert_eq!(wrapped[0].definition.name, "a");
    }

    #[test]
    fn tool_removed_during_execution_not_reported_as_added() {
        let wrapped = wrap_registered_tool(tool("ext-tool"), simple_execute());
        let call = WrappedToolCall {
            tool_call_id: "call-1".into(),
            params: serde_json::json!({}),
            // "read" removed, "new-tool" added.
            active_tools_before: vec!["bash".into(), "read".into()],
            active_tools_after: vec!["bash".into(), "new-tool".into()],
        };
        let result = (wrapped.execute)(call);
        assert_eq!(result.added_tool_names, vec!["new-tool"]);
    }
}
