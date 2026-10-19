//! 上游 `vectorcraft-mcp` 与宿主 MCP 契约之间的薄适配（见宿主 spec §5.4）。
//!
//! **有意不做 UI-only 工具过滤**：上游在调用点运行时判断（以 `Backend::has_ui()`
//! 为准），headless 下恒为 false。按名字静态过滤会在两种情形上出错：
//!   1. 同名工具在不同插件里可用性相反（DesignCraft 的 `type_text` 可用，
//!      VectorCraft 的 `type_text` 不可用）
//!   2. 可用性取决于参数（VectorCraft 的 `screenshot` 仅 `window:true` 需 UI）
//! 因此原样透传，把失败交回上游给出的可读错误。

use serde_json::{json, Value};
use vectorcraft_mcp::{call_tool, tool_definitions, Headless};

/// 宿主 `__otools_mcp_tools__` 的响应体。
pub fn tools_payload() -> Value {
    json!({ "tools": tool_definitions() })
}

/// 进程内**常驻**的 headless 会话。
///
/// 上游 `Headless` 持有 Session，而工具序列有状态（draw_shape → inspect_document
/// 依赖同一个 Session）。每次调用新建 `Headless` 会让第二次调用面对空会话。
///
/// 用 `with_document()` 而不是 `new()`：上游自己的 MCP server 就是这么建的
/// （`crates/mcp/src/tests.rs: server() -> Server::new(Box::new(Headless::with_document()))`）。
/// `new()` 的会话没有活动文档，`draw_shape` 会直接报 "no document open"；
/// 用 `with_document()` 才与外部 AI 连上游 server 时的行为一致。
static HEADLESS: std::sync::OnceLock<std::sync::Mutex<Headless>> = std::sync::OnceLock::new();

fn headless() -> &'static std::sync::Mutex<Headless> {
    HEADLESS.get_or_init(|| std::sync::Mutex::new(Headless::with_document()))
}

/// 关掉会话（供宿主热更新前的停机握手调用）。
pub fn shutdown_headless() {
    if let Some(lock) = HEADLESS.get() {
        if let Ok(mut backend) = lock.lock() {
            *backend = Headless::with_document();
        }
    }
}

/// 宿主 `__otools_mcp_call__` 的响应体。
pub fn call_payload(name: &str, arguments: &Value) -> Value {
    let mut guard = match headless().lock() {
        Ok(guard) => guard,
        Err(_) => {
            return json!({
                "content": [{ "type": "text", "text": "headless 会话状态已损坏，请重启 OTools" }],
                "isError": true
            })
        }
    };
    call_tool(&mut *guard, name, arguments).to_value()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // 这些用例共享进程内的常驻 headless 会话（这本身就是被测行为），必须串行执行。
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn tools_payload_wraps_upstream_definitions() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let value = tools_payload();
        let tools = value
            .get("tools")
            .and_then(|v| v.as_array())
            .expect("tools array");
        assert!(!tools.is_empty(), "tool table should not be empty");
        for tool in tools {
            assert!(
                tool.get("name").and_then(|v| v.as_str()).is_some(),
                "every tool needs a name: {tool}"
            );
        }
    }

    #[test]
    fn tools_payload_passes_through_ui_only_tools() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // 有意**不过滤** UI-only 工具：上游在调用点判断，且 screenshot 的可用性
        // 还取决于参数（仅 window:true 需要 UI），静态过滤会出错。
        let value = tools_payload();
        let names: Vec<&str> = value["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t.get("name").and_then(|v| v.as_str()))
            .collect();
        assert!(names.contains(&"ui_screenshot"), "got {names:?}");
        assert!(names.contains(&"screenshot"), "got {names:?}");
    }

    #[test]
    fn call_unknown_tool_returns_is_error() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let result = call_payload("definitely_not_a_tool", &json!({}));
        assert_eq!(result["isError"], true, "result: {result}");
    }

    #[test]
    fn call_read_only_tool_succeeds_headless() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // list_commands 在 headless 下可用。
        let result = call_payload("list_commands", &json!({}));
        assert_eq!(result["isError"], false, "result: {result}");
        assert!(
            result.get("content").and_then(|v| v.as_array()).is_some(),
            "must return MCP content array: {result}"
        );
    }

    #[test]
    fn session_state_persists_across_calls() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // 防止「每次调用新建 Headless」回归：先画一个对象，再 inspect 必须看得到。
        let drawn = call_payload(
            "draw_shape",
            &json!({"shape": "rectangle", "x": 10, "y": 10, "width": 20, "height": 20}),
        );
        assert_eq!(
            drawn["isError"], false,
            "draw_shape should succeed: {drawn}"
        );

        let inspected = call_payload("inspect_document", &json!({}));
        assert_eq!(
            inspected["isError"], false,
            "inspect_document must see the object drawn in the previous call: {inspected}"
        );
    }
}
