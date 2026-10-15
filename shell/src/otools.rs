//! OTools 宿主桥：把「文件从哪来、存到哪去」换成 OTools 宿主的对话框与磁盘读写。
//!
//! 上游 `apps/vectorcraft-web` 在浏览器里没有文件系统：打开靠 `<input type=file>`，
//! 保存靠浏览器下载。OTools 宿主注入的 `window.otools` 提供了真正的系统文件对话框与
//! `readHostFile` / `writeHostFile`（底层是宿主的 `tools_webview_read_file` /
//! `tools_webview_write_file`，无路径白名单限制），所以本模块是插件**唯一**需要新写的
//! Rust 代码：其余全部复用上游 crate。
//!
//! 与上游的对接点都在上游设计好的 `Services` 钩子上，上游源码零改动：
//! - `Services::open_async`  —— 「文件 ▸ 打开」触发宿主打开对话框，字节进 inbox
//! - `Services::place_async` —— 「置入」触发宿主对话框，字节进 place_inbox
//! - `Services::download`    —— 保存/导出时把字节交给宿主写盘（错误会显示在界面上）
//!
//! JS 侧的实现见 `shell/index.html` 里的 `window.vectorcraftOtools`。

use std::cell::RefCell;

use base64::Engine as _;
use vectorcraft_ui_egui::{
    Inbox,
    place::{PlaceArrival, PlaceInbox},
};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::js_sys;

/// 「文件 ▸ 打开」的扩展名过滤，与上游 `io::open_dialog` 用的那一份一致。
const OPEN_EXTS: &[&str] = &["ai", "svg", "svgz", "pdf", "eps", "ps", "dxf", "cdr", "sketch", "fig", "png", "jpg", "jpeg", "webp", "tiff", "psd"];
/// 「置入」的图片/文件过滤（上游 place 分支）。
const PLACE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "tif", "tiff", "bmp", "psd", "svg", "pdf", "ai", "eps"];

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = vectorcraftOtools, js_name = "available")]
    fn host_available() -> bool;

    #[wasm_bindgen(js_namespace = vectorcraftOtools, js_name = "theme")]
    fn host_theme() -> String;

    /// 宿主界面语言（BCP 47，如 `zh-CN`）：OTools 注入到插件 webview 的
    /// `window.__OTOOLS_LOCALE__.locale` / `document.documentElement.lang`。
    #[wasm_bindgen(js_namespace = vectorcraftOtools, js_name = "language")]
    fn host_language_raw() -> String;

    /// `{title, extensions}` → `[{name, path, dataBase64}]`。
    #[wasm_bindgen(js_namespace = vectorcraftOtools, js_name = "openFiles")]
    fn host_open_files(request: JsValue) -> js_sys::Promise;

    /// `{name, dataBase64, forcePick}` → `{path}` 或 `null`（用户取消）。
    #[wasm_bindgen(js_namespace = vectorcraftOtools, js_name = "saveFile")]
    fn host_save_file(request: JsValue) -> js_sys::Promise;
}

thread_local! {
    static INBOX: RefCell<Option<Inbox>> = const { RefCell::new(None) };
    static PLACE: RefCell<Option<PlaceInbox>> = const { RefCell::new(None) };
    static REPAINT: RefCell<Option<egui::Context>> = const { RefCell::new(None) };
}

/// 宿主是否可用（不在 OTools 里时退回上游的浏览器行为：下载文件）。
pub fn available() -> bool {
    host_available()
}

/// OTools 的界面主题，用于给 egui 一个初始主题（`""` = 跟随系统）。
pub fn theme() -> String {
    host_theme()
}

/// 宿主界面语言（BCP 47，如 `zh-CN`）：跟随 OTools 宿主语言切换。
pub fn host_language() -> String {
    host_language_raw()
}

/// 社区/外部链接（Discord、ArtCraft 官网、上游 GitHub）：在 OTools 插件里点这些
/// 会把用户拽到外部，让插件不纯粹。`open_url` 钩子对命中者返回 `Ok(())` 静默吞掉。
pub fn is_community_url(url: &str) -> bool {
    url.contains("discord.gg") || url.contains("getartcraft.com") || url.contains("github.com/storytold")
}

pub fn install(inbox: Inbox, place: PlaceInbox, ctx: egui::Context) {
    INBOX.with(|slot| *slot.borrow_mut() = Some(inbox));
    PLACE.with(|slot| *slot.borrow_mut() = Some(place));
    REPAINT.with(|slot| *slot.borrow_mut() = Some(ctx));
}

fn repaint() {
    REPAINT.with(|slot| {
        if let Some(ctx) = slot.borrow().as_ref() {
            ctx.request_repaint();
        }
    });
}

/// `Services::open_async`：宿主打开对话框，读到的字节在下一帧由应用打开。
pub fn pick_open() {
    open_into("打开矢量图", OPEN_EXTS, false);
}

/// `Services::place_async`：宿主打开对话框，读到的字节在下一帧被「置入」。
pub fn pick_place() {
    open_into("置入文件", PLACE_EXTS, true);
}

fn open_into(title: &str, exts: &[&str], for_place: bool) {
    let request = open_request(title, exts);
    wasm_bindgen_futures::spawn_local(async move {
        let files = match open_files(&request).await {
            Ok(files) => files,
            Err(e) => {
                log::error!("打开失败：{e}");
                return;
            }
        };
        for (name, bytes) in files {
            if for_place {
                PLACE.with(|slot| {
                    if let Some(inbox) = slot.borrow().as_ref() {
                        inbox.lock().unwrap_or_else(|e| e.into_inner()).push(PlaceArrival { name, bytes, drop: None });
                    }
                });
            } else {
                INBOX.with(|slot| {
                    if let Some(inbox) = slot.borrow().as_ref() {
                        inbox.lock().unwrap_or_else(|e| e.into_inner()).push((name, bytes));
                    }
                });
            }
        }
        repaint();
    });
}

/// `Services::download`：把保存/导出的字节交给宿主写盘。
///
/// 上游的钩子是同步的 `fn`，而宿主的对话框是 Promise，所以这里发起一次后台任务并
/// 立刻返回 `Ok`（与上游网页版「下载已开始即算已保存」的语义一致）。
pub fn deliver(name: &str, bytes: &[u8]) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("需要一个文件名".into());
    }
    let request = save_request(name, &base64::engine::general_purpose::STANDARD.encode(bytes), true);
    let name = name.to_string();
    wasm_bindgen_futures::spawn_local(async move {
        match save_file(&request).await {
            Ok(Some(path)) => log::info!("saved {name} -> {path}"),
            Ok(None) => log::error!("保存已取消：{name}"),
            Err(e) => log::error!("保存失败：{e}"),
        }
    });
    Ok(())
}

fn open_request(title: &str, exts: &[&str]) -> JsValue {
    let request = js_sys::Object::new();
    let arr = js_sys::Array::new();
    for e in exts {
        arr.push(&JsValue::from_str(e));
    }
    let _ = js_sys::Reflect::set(&request, &JsValue::from_str("title"), &JsValue::from_str(title));
    let _ = js_sys::Reflect::set(&request, &JsValue::from_str("extensions"), &arr);
    request.into()
}

async fn open_files(request: &JsValue) -> Result<Vec<(String, Vec<u8>)>, String> {
    let value = JsFuture::from(host_open_files(request.clone())).await.map_err(js_error)?;
    let mut out = Vec::new();
    for entry in js_sys::Array::from(&value).iter() {
        let name = field(&entry, "name").unwrap_or_else(|| "document.svg".to_string());
        let data = field(&entry, "dataBase64").unwrap_or_default();
        let bytes = base64_decode(&data)?;
        out.push((name, bytes));
    }
    Ok(out)
}

fn save_request(name: &str, data: &str, force_pick: bool) -> JsValue {
    let request = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&request, &JsValue::from_str("name"), &JsValue::from_str(name));
    let _ = js_sys::Reflect::set(&request, &JsValue::from_str("dataBase64"), &JsValue::from_str(data));
    let _ = js_sys::Reflect::set(&request, &JsValue::from_str("forcePick"), &JsValue::from_bool(force_pick));
    request.into()
}

async fn save_file(request: &JsValue) -> Result<Option<String>, String> {
    let value = JsFuture::from(host_save_file(request.clone())).await.map_err(js_error)?;
    if value.is_null() || value.is_undefined() {
        return Ok(None);
    }
    Ok(field(&value, "path"))
}

fn base64_decode(data: &str) -> Result<Vec<u8>, String> {
    if data.is_empty() {
        return Err("宿主返回的文件内容为空".into());
    }
    base64::engine::general_purpose::STANDARD.decode(data).map_err(|e| format!("宿主返回的内容不是合法的 base64：{e}"))
}

fn field(value: &JsValue, key: &str) -> Option<String> {
    js_sys::Reflect::get(value, &JsValue::from_str(key)).ok()?.as_string()
}

fn js_error(e: JsValue) -> String {
    e.as_string().unwrap_or_else(|| format!("{e:?}"))
}
