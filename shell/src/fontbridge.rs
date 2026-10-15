//! OTools 宿主字体桥：把系统字体接进 VectorCraft 的字体库。
//!
//! wasm 里读不到本机字体（上游的字体扫描在 wasm 直接跳过，字体目录恒空），而设计软件的
//! 「文档内容字体」必须是用户系统里装的任意字体——内嵌字体只覆盖得了 UI 层。OTools 宿主能
//! 枚举并读系统字体，这里把两者的路接起来：
//!
//! 1. 启动时 `vectorcraftOtools.listSystemFonts()` 取字体目录（族名/样式/路径，宿主
//!    只解析每个字体文件的 name 表，几 KB/文件，不读整个字体），喂给上游字体库
//!    （`FontDb::set_system_catalog`）——字体菜单立刻能列出系统字体。
//! 2. 应用第一次解析某个还没加载的系统字体时（上游 `FontDb::face` miss），族名会进
//!    待加载队列；本模块每帧取队列，按路径读字节（复用宿主的 `readHostFile`，base64
//!    二进制安全），`FontDb::add_font` 装回库并请求重绘。下一次布局就用上真字体了。
//!
//! 不在 OTools 里（把 dist 拖进普通浏览器）时没有宿主桥，本模块整体静默：字体库里
//! 只有上游内嵌字体，与此前行为一致。UI 字体走内嵌 fallback，与此无关。

use std::cell::RefCell;

use base64::Engine as _;
use vectorcraft_text::{FontDb, SystemFont};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::js_sys;

#[wasm_bindgen]
extern "C" {
    /// 宿主枚举的系统字体目录：`[{family, style, path, index, size}]`。
    #[wasm_bindgen(js_namespace = vectorcraftOtools, js_name = "listSystemFonts")]
    fn host_list_system_fonts() -> js_sys::Promise;

    /// 读宿主磁盘文件：`{path, name, dataBase64, …}`（与打开文档同一条路）。
    #[wasm_bindgen(js_namespace = vectorcraftOtools, js_name = "readHostFile")]
    fn host_read_file(path: &str) -> js_sys::Promise;

    /// 探针专用（无头验证字体桥用）：注入的脚本经它请求「解析某个族」或「列出字体族」，
    /// 正常运行时恒返回 `null`。形状：`["resolve", family, style]` / `["families", family]` / `null`。
    /// `catch`：本调用每帧都发，JS 侧万一缺失时拿 Err 静默跳过，不把异常抛进 wasm。
    #[wasm_bindgen(js_namespace = vectorcraftOtools, js_name = "probeRequest", catch)]
    fn host_probe_request() -> Result<JsValue, JsValue>;
}

thread_local! {
    /// 宿主给的字体目录（族名 → 字体文件路径）。只存路径不存字节：字节按需读。
    static CATALOG: RefCell<Vec<SystemFont>> = const { RefCell::new(Vec::new()) };
    /// 已经读过字节的字体文件路径，避免同一文件重复读（量小，线性查）。
    static READ: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// 一次 poll 最多发起读取的字体族数（一个族常带几个样式文件，已是几次 IPC）。
const MAX_FAMILIES_PER_POLL: usize = 4;

/// 启动时取一次系统字体目录（异步）。不在 OTools 里时什么都不做。
pub fn fetch_catalog() {
    if !crate::otools::available() {
        return;
    }
    wasm_bindgen_futures::spawn_local(async move {
        match JsFuture::from(host_list_system_fonts()).await {
            Ok(value) => {
                let mut fonts = Vec::new();
                for entry in js_sys::Array::from(&value).iter() {
                    let family = field(&entry, "family").unwrap_or_default();
                    let style = field(&entry, "style").unwrap_or_else(|| "Regular".into());
                    let path = field(&entry, "path").unwrap_or_default();
                    if family.trim().is_empty() || path.trim().is_empty() {
                        continue;
                    }
                    // 菜单排序用的体重/斜体：宿主只给样式名，按上游同样的规则推。
                    let weight = vectorcraft_text::style_weight(&style);
                    let italic = {
                        let s = style.to_lowercase();
                        s.contains("italic") || s.contains("oblique")
                    };
                    fonts.push(SystemFont { family, style, path, weight, italic });
                }
                log::info!("宿主字体目录：{} 个面", fonts.len());
                CATALOG.with(|c| *c.borrow_mut() = fonts.clone());
                FontDb::global().set_system_catalog(fonts);
                crate::otools::repaint();
            }
            Err(e) => log::warn!("枚举系统字体失败：{}", js_error(e)),
        }
    });
}

/// 每帧调用：把应用要过的系统字体读进来（有界批量，读完请求重绘）。
pub fn poll(ctx: &egui::Context) {
    drain_probe_requests();
    let pending = vectorcraft_text::take_pending_fonts();
    if pending.is_empty() {
        return;
    }
    for family in pending.into_iter().take(MAX_FAMILIES_PER_POLL) {
        // 这个族的全部字体文件（Regular/Bold/Italic… 常是几个文件）。
        let todo: Vec<String> = CATALOG.with(|c| {
            c.borrow()
                .iter()
                .filter(|f| f.family.eq_ignore_ascii_case(&family))
                .map(|f| f.path.clone())
                .filter(|p| !READ.with(|r| r.borrow().contains(p)))
                .collect()
        });
        if todo.is_empty() {
            continue;
        }
        let ctx = ctx.clone();
        wasm_bindgen_futures::spawn_local(async move {
            for path in todo {
                match JsFuture::from(host_read_file(&path)).await {
                    Ok(value) => {
                        let data = field(&value, "dataBase64").unwrap_or_default();
                        match base64::engine::general_purpose::STANDARD.decode(&data) {
                            Ok(bytes) => {
                                let added = FontDb::global().add_font(bytes);
                                READ.with(|r| {
                                    let mut read = r.borrow_mut();
                                    if !read.contains(&path) {
                                        read.push(path.clone());
                                    }
                                });
                                log::info!("系统字体 {family}（{path}）：+{added} 面");
                            }
                            Err(e) => log::warn!("字体 {path} 内容不是合法 base64：{e}"),
                        }
                    }
                    Err(e) => log::warn!("读字体 {path} 失败：{}", js_error(e)),
                }
            }
            ctx.request_repaint();
        });
    }
}

/// 探针专用：处理注入脚本的请求。`probeRequest()` 正常返回 `null`，这条路整体是
/// 空转（一次 wasm→JS 调用，可忽略）；只有无头验证时注入的脚本会经它发起请求。
fn drain_probe_requests() {
    let Ok(request) = host_probe_request() else { return };
    let Some(arr) = request.dyn_ref::<js_sys::Array>() else { return };
    let command = arr.get(0).as_string().unwrap_or_default();
    match command.as_str() {
        // ["resolve", family, style]：解析一次并打日志（验证回落/命中）。
        "resolve" if arr.length() >= 3 => {
            let family = arr.get(1).as_string().unwrap_or_default();
            let style = arr.get(2).as_string().unwrap_or_default();
            if family.is_empty() {
                return;
            }
            let hit = FontDb::global()
                .face(&family, &style)
                .map(|f| format!("{} {}", f.family, f.style))
                .unwrap_or_else(|| "(none)".into());
            log::info!("PROBE resolve {family}/{style} -> {hit}");
        }
        // ["families", family]：字体菜单列不列这个族（验证目录到达）。
        "families" if arr.length() >= 2 => {
            let family = arr.get(1).as_string().unwrap_or_default();
            let listed = FontDb::global().menu_family_list().iter().any(|f| f.eq_ignore_ascii_case(&family));
            let count = FontDb::global().menu_family_list().len();
            log::info!("PROBE families contains={listed} count={count}");
        }
        _ => {}
    }
}

fn field(value: &JsValue, key: &str) -> Option<String> {
    js_sys::Reflect::get(value, &JsValue::from_str(key)).ok()?.as_string()
}

fn js_error(e: JsValue) -> String {
    e.as_string().unwrap_or_else(|| format!("{e:?}"))
}
