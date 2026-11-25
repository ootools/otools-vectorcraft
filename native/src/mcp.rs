//! 上游 `vectorcraft-mcp` 与宿主 MCP 契约之间的薄适配（见宿主 spec §5.4）。
//!
//! **有意不做 UI-only 工具过滤**：上游在调用点运行时判断（以 `Backend::has_ui()`
//! 为准），headless 下恒为 false。按名字静态过滤会在两种情形上出错：
//!   1. 同名工具在不同插件里可用性相反（DesignCraft 的 `type_text` 可用，
//!      VectorCraft 的 `type_text` 不可用）
//!   2. 可用性取决于参数（VectorCraft 的 `screenshot` 仅 `window:true` 需 UI）
//! 因此原样透传，把失败交回上游给出的可读错误。

use serde_json::{json, Value};
use vectorcraft_mcp::{tool_definitions, Headless, Server};

/// 宿主 `__otools_mcp_tools__` 的响应体。
pub fn tools_payload() -> Value {
    json!({ "tools": tool_definitions() })
}

enum WorkerRequest {
    Mcp {
        method: String,
        params: Value,
        reply: std::sync::mpsc::Sender<Result<Value, String>>,
    },
    Stop,
}

struct Worker {
    sender: std::sync::mpsc::Sender<WorkerRequest>,
    thread: std::thread::JoinHandle<()>,
}

/// Headless 的渲染缓存含 Rc，必须在同一线程创建、使用和释放。
/// 静态槽只持有可跨线程发送的请求通道；工具和 resources 共用唯一 Server。
static WORKER: std::sync::Mutex<Option<Worker>> = std::sync::Mutex::new(None);

fn start_worker() -> Result<Worker, String> {
    let (sender, receiver) = std::sync::mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("vectorcraft-mcp".to_string())
        .spawn(move || {
            // with_document 与上游 MCP 的初始会话一致，调用者线程不接触 Headless。
            let mut server = Server::new(Box::new(Headless::with_document()));
            while let Ok(message) = receiver.recv() {
                match message {
                    WorkerRequest::Mcp {
                        method,
                        params,
                        reply,
                    } => {
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            request(&mut server, &method, params)
                        }));
                        let result = match result {
                            Ok(result) => result,
                            Err(_) => {
                                server = Server::new(Box::new(Headless::with_document()));
                                Err("MCP 会话内部错误，已重置会话".to_string())
                            }
                        };
                        let _ = reply.send(result);
                    }
                    WorkerRequest::Stop => break,
                }
            }
        })
        .map_err(|error| format!("无法启动 MCP 工作线程: {error}"))?;
    Ok(Worker { sender, thread })
}

/// 走一次上游 MCP 分发，取 result；上游继续负责参数、资源与 prompt 的解释。
fn request(server: &mut Server, method: &str, params: Value) -> Result<Value, String> {
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

pub fn rpc(method: &str, params: Value) -> Result<Value, String> {
    // 请求完成前持锁，使 shutdown 等待在途调用，并阻止新旧会话同时存活。
    let mut slot = WORKER
        .lock()
        .map_err(|_| "MCP 工作线程状态已损坏".to_string())?;
    if slot.is_none() {
        *slot = Some(start_worker()?);
    }
    let worker = slot.as_ref().expect("worker initialized");
    let (reply, response) = std::sync::mpsc::channel();
    worker
        .sender
        .send(WorkerRequest::Mcp {
            method: method.to_string(),
            params,
            reply,
        })
        .map_err(|_| "MCP 工作线程已停止".to_string())?;
    response
        .recv()
        .map_err(|_| "MCP 工作线程未返回结果".to_string())?
}

/// 宿主 `__otools_mcp_resources__` 的响应体（4 个固定资源）。
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

/// 宿主 `__otools_mcp_prompts__` 的响应体（上游 8 个设计工作流）。
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

/// 热更新握手必须等线程退出再返回，防止卸载动态库后仍有旧代码运行。
pub fn shutdown_headless() -> Result<(), String> {
    let mut slot = WORKER
        .lock()
        .map_err(|_| "MCP 工作线程状态已损坏".to_string())?;
    if let Some(worker) = slot.take() {
        let _ = worker.sender.send(WorkerRequest::Stop);
        worker
            .thread
            .join()
            .map_err(|_| "MCP 工作线程异常退出".to_string())?;
    }
    Ok(())
}

/// 宿主 __otools_mcp_call__ 的响应体，保留 MCP 工具错误格式。
pub fn call_payload(name: &str, arguments: &Value) -> Value {
    rpc(
        "tools/call",
        json!({ "name": name, "arguments": arguments }),
    )
    .unwrap_or_else(|error| {
        json!({
            "content": [{ "type": "text", "text": error }],
            "isError": true,
        })
    })
}

// MCP 用例和 FFI 停机用例共用会话，跨模块也必须串行。
#[cfg(test)]
pub(crate) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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

    fn object_count() -> u64 {
        let inspected = call_payload("doc_inspect", &json!({"depth": 0}));
        assert_eq!(inspected["isError"], false, "{inspected}");
        let text = inspected["content"][0]["text"]
            .as_str()
            .expect("document JSON");
        let document: Value = serde_json::from_str(text).expect("document JSON");
        document["objects"].as_u64().expect("object count")
    }

    #[test]
    fn callers_on_different_threads_share_one_document() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        shutdown_headless().expect("stop worker");
        let before = object_count();
        let drawn = std::thread::spawn(|| {
            call_payload(
                "draw_shape",
                &json!({"shape": "rectangle", "x": 10, "y": 10, "width": 20, "height": 20}),
            )
        })
        .join()
        .expect("drawing caller");
        assert_eq!(drawn["isError"], false, "{drawn}");
        let after = std::thread::spawn(object_count)
            .join()
            .expect("inspection caller");
        assert_eq!(
            after,
            before + 1,
            "the second caller must see the first caller's shape"
        );
        let resource = read_resource_payload("vectorcraft://document").expect("document resource");
        let document: Value = serde_json::from_str(
            resource["contents"][0]["text"]
                .as_str()
                .expect("document JSON"),
        )
        .expect("resource JSON");
        assert_eq!(
            document["objects"],
            json!(after),
            "resources and tools must share the document"
        );
    }

    #[test]
    fn shutdown_clears_the_document_and_allows_a_new_session() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        shutdown_headless().expect("stop worker");
        let empty = object_count();
        let drawn = call_payload(
            "draw_shape",
            &json!({"shape": "rectangle", "x": 10, "y": 10, "width": 20, "height": 20}),
        );
        assert_eq!(drawn["isError"], false, "{drawn}");
        assert_eq!(object_count(), empty + 1);
        shutdown_headless().expect("stop worker");
        shutdown_headless().expect("stop worker");
        assert_eq!(
            object_count(),
            empty,
            "shutdown must release the old session"
        );
    }

    // ── resources ──────────────────────────────────────────────────────────
    //
    // 上游的 `resources/*` 与 `prompts/*` 都在 `Server::handle` 里，适配层通过
    // 专用线程转发。这几条锁的是「转发真的通」，且 URI 模板的 {id} 解析（上游的
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
        assert_eq!(uris.len(), 4, "got {uris:?}");
        for expected in [
            "vectorcraft://document",
            "vectorcraft://document/json",
            "vectorcraft://commands",
            "vectorcraft://commands/reference",
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
        // 上游 0.24.1 的 8 个设计工作流。
        assert_eq!(names.len(), 8, "got {names:?}");
        for expected in [
            "poster",
            "icon-set",
            "recolor",
            "trace-and-style",
            "export-set",
            "check-spelling",
            "variables-bind",
            "path-tools",
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
