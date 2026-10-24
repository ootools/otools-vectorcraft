//! 上游 `vectorcraft-mcp` 与宿主 MCP 契约之间的薄适配（见宿主 spec §5.4）。
//!
//! **有意不做 UI-only 工具过滤**：上游在调用点运行时判断（以 `Backend::has_ui()`
//! 为准），headless 下恒为 false。按名字静态过滤会在两种情形上出错：
//!   1. 同名工具在不同插件里可用性相反（DesignCraft 的 `type_text` 可用，
//!      VectorCraft 的 `type_text` 不可用）
//!   2. 可用性取决于参数（VectorCraft 的 `screenshot` 仅 `window:true` 需 UI）
//! 因此原样透传，把失败交回上游给出的可读错误。

use serde_json::{json, Value};
use vectorcraft_mcp::{call_tool, tool_definitions, Backend, Headless, Server};

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
static HEADLESS: std::sync::OnceLock<std::sync::Arc<std::sync::Mutex<Headless>>> =
    std::sync::OnceLock::new();

fn headless() -> &'static std::sync::Arc<std::sync::Mutex<Headless>> {
    HEADLESS.get_or_init(|| std::sync::Arc::new(std::sync::Mutex::new(Headless::with_document())))
}

/// 把常驻会话包成上游 `Server` 能接受的 `Backend`。
///
/// `Server::new` 要求 `Box<dyn Backend>` 的所有权，交出去就再也拿不回来（会话状态
/// 会丢）。所以每次请求都新建一个只持有 `Arc<Mutex<Headless>>` 的代理 Server：
/// 资源模板、`{id}` 解析、percent-decoding、错误码全部由上游代码产生，适配层不再
/// 复制一份映射——那种复制正是上游改一处、插件悄悄过期的地方。
struct Proxy(std::sync::Arc<std::sync::Mutex<Headless>>);

impl Proxy {
    fn with<T>(&self, f: impl FnOnce(&mut Headless) -> T) -> Result<T, String> {
        let mut guard = self
            .0
            .lock()
            .map_err(|_| "headless 会话状态已损坏，请重启 OTools".to_string())?;
        Ok(f(&mut guard))
    }
}

impl Backend for Proxy {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.with(|h| h.call(method, params))?
    }

    fn has_ui(&self) -> bool {
        false
    }

    fn describe(&self) -> String {
        "otools-vectorcraft native (headless)".to_string()
    }
}

/// 走一次上游 MCP 分发，取 `result`。
pub fn rpc(method: &str, params: Value) -> Result<Value, String> {
    let mut server = Server::new(Box::new(Proxy(headless().clone())));
    let reply = server
        .handle(json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }))
        .ok_or_else(|| format!("上游对 {method} 没有应答"))?;
    if let Some(error) = reply.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("上游返回错误");
        return Err(message.to_string());
    }
    reply
        .get("result")
        .cloned()
        .ok_or_else(|| format!("{method} 的应答缺少 result"))
}

/// 宿主 `__otools_mcp_resources__` 的响应体（3 个固定资源）。
pub fn resources_payload() -> Result<Value, String> {
    rpc("resources/list", json!({}))
}

/// 宿主 `__otools_mcp_resource_templates__`（4 个模板：object / command / effect / swatch）。
pub fn resource_templates_payload() -> Result<Value, String> {
    rpc("resources/templates/list", json!({}))
}

/// 宿主 `__otools_mcp_read_resource__`。模板 URI 的 `{id}` 解析与
/// percent-decoding 都在上游完成。
pub fn read_resource_payload(uri: &str) -> Result<Value, String> {
    rpc("resources/read", json!({ "uri": uri }))
}

/// 宿主 `__otools_mcp_prompts__` 的响应体（上游 4 个设计工作流）。
pub fn prompts_payload() -> Result<Value, String> {
    rpc("prompts/list", json!({}))
}

/// 宿主 `__otools_mcp_get_prompt__` 的响应体。模板渲染与默认值都在上游。
pub fn get_prompt_payload(name: &str, arguments: &Value) -> Result<Value, String> {
    rpc(
        "prompts/get",
        json!({ "name": name, "arguments": arguments }),
    )
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

    // ── resources ──────────────────────────────────────────────────────────
    //
    // 上游的 `resources/*` 与 `prompts/*` 都在 `Server::handle` 里，适配层通过
    // Proxy 转发。这几条锁的是「转发真的通」，且 URI 模板的 {id} 解析（上游的
    // percent-decoding 也在这里）没有在转发中丢掉。

    #[test]
    fn resources_list_matches_upstream() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let value = resources_payload().expect("resources/list");
        let uris: Vec<&str> = value["resources"]
            .as_array()
            .expect("resources array")
            .iter()
            .filter_map(|r| r.get("uri").and_then(Value::as_str))
            .collect();
        assert_eq!(uris.len(), 3, "got {uris:?}");
        for expected in [
            "vectorcraft://document",
            "vectorcraft://document/json",
            "vectorcraft://commands",
        ] {
            assert!(uris.contains(&expected), "missing {expected} in {uris:?}");
        }
    }

    #[test]
    fn resource_templates_come_from_upstream() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // 上游有 4 个模板（object / command / effect / swatch）。这是 vectorcraft
        // 与其它 craft 的关键差异：少一个就是转发链路漏了。
        let value = resource_templates_payload().expect("resources/templates/list");
        let templates = value
            .get("resourceTemplates")
            .and_then(Value::as_array)
            .expect("resourceTemplates array");
        let names: Vec<&str> = templates
            .iter()
            .filter_map(|t| t.get("name").and_then(Value::as_str))
            .collect();
        assert_eq!(names.len(), 4, "got {names:?}");
        for expected in ["object", "command", "effect", "swatch"] {
            assert!(names.contains(&expected), "missing {expected} in {names:?}");
        }
    }

    #[test]
    fn read_resource_round_trips_and_rejects_unknown_uri() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let contents = read_resource_payload("vectorcraft://commands")
            .expect("read commands resource")
            .get("contents")
            .and_then(Value::as_array)
            .cloned()
            .expect("contents array");
        let text = contents[0]
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(!text.trim().is_empty(), "commands resource came back empty");

        let error = read_resource_payload("vectorcraft://nope").expect_err("unknown uri must fail");
        assert!(
            error.contains("not found") || error.contains("resource"),
            "error should say the resource is missing: {error}"
        );
    }

    #[test]
    fn prompts_list_matches_upstream() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let value = prompts_payload().expect("prompts/list");
        let names: Vec<&str> = value["prompts"]
            .as_array()
            .expect("prompts array")
            .iter()
            .filter_map(|p| p.get("name").and_then(Value::as_str))
            .collect();
        // 上游 5 个设计工作流。
        assert_eq!(names.len(), 5, "got {names:?}");
        for expected in [
            "poster",
            "icon-set",
            "recolor",
            "trace-and-style",
            "export-set",
        ] {
            assert!(names.contains(&expected), "missing {expected} in {names:?}");
        }
    }

    #[test]
    fn get_prompt_renders_messages_and_rejects_unknown_name() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // palette 有固定取值集，用其中一个，避免测试依赖上游对非法值的容忍度。
        let rendered =
            get_prompt_payload("recolor", &json!({"palette": "cluster"})).expect("render recolor");
        let messages = rendered
            .get("messages")
            .and_then(Value::as_array)
            .expect("messages array");
        assert!(
            !messages.is_empty(),
            "rendered prompt has no messages: {rendered}"
        );
        let text = messages[0]
            .pointer("/content/text")
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(text.contains("cluster"), "argument not substituted: {text}");

        let error =
            get_prompt_payload("definitely_not_a_prompt", &json!({})).expect_err("unknown prompt");
        assert!(
            error.contains("unknown prompt"),
            "error should say the prompt does not exist: {error}"
        );
    }
}
