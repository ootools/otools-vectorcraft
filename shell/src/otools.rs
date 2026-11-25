//! OTools 文件服务与字体桥。框架 Files 请求通过宿主对话框异步完成，
//! 打开答复 Contents，保存路径答复 Saved，取消与错误保留各自结果。
//! Services::download 仍用于网页版保存/导出；JS 桥在 shell/index.html。

use std::cell::RefCell;

use base64::Engine as _;
use vectorcraft_ui_egui::{
    Inbox,
    place::PlaceInbox,
};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::js_sys;

/// 「文件 ▸ 打开」的扩展名过滤，与上游 `io::open_dialog` 用的那一份一致。
/// 「置入」的图片/文件过滤（上游 place 分支）。

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = vectorcraftOtools, js_name = "pickSavePath")]
    fn host_pick_save_path(request: JsValue) -> js_sys::Promise;
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

    /// 宿主投递的「打开方式」：JS 侧已把文件读成字节攒好，这里一次取走。
    #[wasm_bindgen(js_namespace = vectorcraftOtools, js_name = "takeDelivered")]
    fn take_delivered_raw() -> js_sys::Array;
}

/// 宿主投递来的一份文件（JS 侧已读成字节）。
pub struct Delivered {
    pub name: String,
    pub bytes: Vec<u8>,
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

pub(crate) fn repaint() {
    REPAINT.with(|slot| {
        if let Some(ctx) = slot.borrow().as_ref() {
            ctx.request_repaint();
        }
    });
}

/// 取走宿主投递的文件：下一帧由 `WebShell::logic` 推进 inbox，
/// 与「文件 ▸ 打开」和拖放同一条路（`io::open_bytes` 的格式分流全都在里面）。
///
/// 方向与 [`deliver`] 相反：宿主在系统里双击已绑定扩展名的文件 → 投递队列 →
/// 本函数取走 → 上游 `io::open_bytes`。
pub fn drain_delivered() -> Vec<Delivered> {
    let mut out = Vec::new();
    for item in take_delivered_raw().iter() {
        if !item.is_object() {
            continue;
        }
        let name = field(&item, "name").unwrap_or_else(|| "document.svg".to_string());
        let data = field(&item, "dataBase64").unwrap_or_default();
        match base64_decode(&data) {
            Ok(bytes) => out.push(Delivered { name, bytes }),
            Err(e) => log::error!("宿主投递 {name} 失败：{e}"),
        }
    }
    out
}

/// `Services::download`：把保存/导出的字节交给宿主写盘。
///
/// 上游的钩子是同步的 `fn`，而宿主的对话框是 Promise，所以这里发起一次后台任务并
/// 立刻返回 `Ok`（与上游网页版「下载已开始即算已保存」的语义一致）。
pub fn deliver(name: &str, bytes: &[u8]) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("需要一个文件名".into());
    }
    let request = save_request(name, &base64::engine::general_purpose::STANDARD.encode(bytes), !std::path::Path::new(name).is_absolute());
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


pub struct HostFiles {
    browser: craft::platform::browser_files::BrowserFiles,
    pending: std::sync::Arc<std::sync::Mutex<crate::requests::Requests<craft::host::PickRequest, craft::host::Picked>>>,
    ctx: egui::Context,
}
impl HostFiles {
    pub fn new(ctx: egui::Context) -> Self {
        let wake = ctx.clone();
        Self {
            browser: craft::platform::browser_files::BrowserFiles::new(move || wake.request_repaint()),
            pending: Default::default(), ctx,
        }
    }
    fn issue(&self, request: craft::host::PickRequest) -> craft::host::PickId {
        craft::host::PickId(self.pending.lock().unwrap_or_else(|e| e.into_inner()).issue(request))
    }
}
impl craft::host::Files for HostFiles {
    fn pick(&self, request: craft::host::PickRequest) -> craft::host::PickId {
        if !available() { return self.browser.pick(request); }
        let id = self.issue(request.clone());
        if request.kind == craft::host::PickKind::Folder {
            self.pending.lock().unwrap_or_else(|e| e.into_inner()).finish(id.0, craft::host::Picked::Unavailable("此浏览器插件暂不支持文件夹选择".into()));
            return id;
        }
        if request.kind == craft::host::PickKind::Save {
            let js = save_request(request.file_name.as_deref().unwrap_or("document.svg"), "", true);
            let pending = self.pending.clone(); let ctx = self.ctx.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let result = match JsFuture::from(host_pick_save_path(js)).await {
                    Ok(value) => match value.as_string().filter(|p| !p.is_empty()) {
                        Some(path) => craft::host::Picked::Saved { path: path.into(), file_type: None },
                        None => craft::host::Picked::Cancelled,
                    },
                    Err(e) => craft::host::Picked::Unavailable(js_error(e)),
                };
                pending.lock().unwrap_or_else(|e| e.into_inner()).finish(id.0, result);
                ctx.request_repaint();
            });
            return id;
        }
        let extensions: Vec<&str> = request.filters.iter().flat_map(|f| f.extensions.iter().map(String::as_str)).collect();
        let js = open_request(request.title.as_deref().unwrap_or("打开文件"), &extensions);
        let pending = self.pending.clone(); let ctx = self.ctx.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let result = match open_files(&js).await {
                Ok(files) if files.is_empty() => craft::host::Picked::Cancelled,
                Ok(files) => craft::host::Picked::Contents(files),
                Err(e) => craft::host::Picked::Unavailable(e),
            };
            pending.lock().unwrap_or_else(|e| e.into_inner()).finish(id.0, result);
            ctx.request_repaint();
        });
        id
    }
    fn take_answer(&self, id: craft::host::PickId) -> Option<craft::host::Picked> {
        if id.0 & (1 << 63) == 0 { return self.browser.take_answer(id); }
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).take_answer(id.0)
    }
    fn forget(&self, id: craft::host::PickId) {
        if id.0 & (1 << 63) == 0 { self.browser.forget(id); }
        else { self.pending.lock().unwrap_or_else(|e| e.into_inner()).forget(id.0); }
    }
    fn pending_pick(&self) -> Option<craft::host::PendingPick> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).pending().cloned()
            .map(|request| craft::host::PendingPick { request, sheet: false })
            .or_else(|| self.browser.pending_pick())
    }
    fn read(&self, path: &std::path::Path) -> Result<Vec<u8>, craft::host::FileError> { self.browser.read(path) }
    fn write(&self, path: &std::path::Path, bytes: &[u8], intent: craft::host::WriteIntent) -> Result<bool, craft::host::FileError> {
        if available() { deliver(&path.to_string_lossy(), bytes).map(|_| true).map_err(craft::host::FileError::Other) }
        else { self.browser.write(path, bytes, intent) }
    }
    fn last_folder(&self, purpose: &str) -> Option<std::path::PathBuf> { self.browser.last_folder(purpose) }
    fn save(&self, request: craft::host::PickRequest, bytes: Vec<u8>) -> craft::host::PickId {
        if !available() { return self.browser.save(request, bytes); }
        let id = self.issue(request.clone());
        let js = save_request(request.file_name.as_deref().unwrap_or("document.svg"), &base64::engine::general_purpose::STANDARD.encode(bytes), true);
        let pending = self.pending.clone(); let ctx = self.ctx.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let result = match save_file(&js).await {
                Ok(Some(path)) => craft::host::Picked::Saved { path: path.into(), file_type: None },
                Ok(None) => craft::host::Picked::Cancelled,
                Err(e) => craft::host::Picked::Failed(e),
            };
            pending.lock().unwrap_or_else(|e| e.into_inner()).finish(id.0, result);
            ctx.request_repaint();
        }); id
    }
    fn can_pick(&self) -> bool { false }
    fn browser_saves(&self) -> bool { true }
}
