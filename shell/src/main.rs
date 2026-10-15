//! VectorCraft 的 OTools 插件外壳。
//!
//! 本文件改编自上游 `apps/vectorcraft-web/src/web.rs`（上游 eframe web runner 的外壳）。
//! 改动都在「文件从哪来、存到哪去」上：
//!
//! 1. `services()` 的 `open_async` / `place_async` / `download` 走 [`otools`]
//! 2. 首次启动时用 OTools 的界面主题给 egui 一个初始亮度
//! 3. 不在 OTools 里时（把 dist 拖进普通浏览器）退回浏览器下载
//! 4. 系统字体经宿主字体桥按需装进上游字体库（见 [`fontbridge`]）：设计软件的
//!    「文档内容字体」要能用用户系统里装的任意字体，而 wasm 读不到本机字体
//!
//! 上游那些纯浏览器设施没有移植：GPU 丢失重启、开机字体抓取、多标签页 Web Locks、
//! 粘贴事件。它们在插件里没有意义（见 AGENTS.md 的「已知限制」）。
//! 上游 `crates/*` 一律不改：它们按 upstream.pin 的 commit 拉取，保持原样才能干净地跟上上游。

#[cfg(target_arch = "wasm32")]
mod otools;

#[cfg(target_arch = "wasm32")]
mod fontbridge;

#[cfg(target_arch = "wasm32")]
mod web {
    use std::cell::{Cell, RefCell};

    use vectorcraft_engine::Session;
    use vectorcraft_ui_egui::{Inbox, Services, VectorcraftApp, place::PlaceInbox, theme::Brightness};
    use wasm_bindgen::closure::Closure;
    use wasm_bindgen::JsCast as _;

    const CANVAS_ID: &str = "vectorcraft_canvas";
    const LOADING_ID: &str = "vectorcraft_loading";

    /// 包一层应用：把拖放进来的文件读出来（浏览器只能异步读），交给 inbox。
    struct WebShell {
        app: VectorcraftApp,
        inbox: Inbox,
    }

    thread_local! {
        /// 宿主语言切换后，下一帧把 host locale 写进 interface_language 偏好并重渲染。
        static NEEDS_LANG_SYNC: Cell<bool> = const { Cell::new(true) };
        /// 缓存 egui Context，供语言切换事件触发重绘。
        static APP_CTX: RefCell<Option<egui::Context>> = const { RefCell::new(None) };
    }

    impl eframe::App for WebShell {
        fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
            APP_CTX.with(|c| *c.borrow_mut() = Some(ctx.clone()));
            // 跟随宿主语言：切换后下一帧把 host locale 写进 interface_language 偏好（重渲染）。
            if NEEDS_LANG_SYNC.with(|f| f.replace(false)) {
                let code = host_lang_code(&crate::otools::host_language());
                if self.app.session.prefs.interface_language != code {
                    self.app.session.prefs.interface_language = code;
                }
            }
            let dropped = ctx.input_mut(|i| std::mem::take(&mut i.raw.dropped_files));
            for f in dropped {
                let inbox = self.inbox.clone();
                let ctx = ctx.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    let name = f.path().file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "dropped".into());
                    match f.bytes_async().await {
                        Ok(bytes) => {
                            inbox.lock().unwrap_or_else(|e| e.into_inner()).push((name, bytes));
                            ctx.request_repaint();
                        }
                        Err(e) => log::error!("读拖放文件 {name} 失败：{e}"),
                    }
                });
            }
            self.app.logic(ctx);
            // 系统字体桥：应用要过的系统字体按需读进来（无宿主时整体静默）。
            crate::fontbridge::poll(ctx);
            // 让插件更纯粹：吞掉所有社区/外部链接（Discord、ArtCraft 官网、上游 GitHub），
            // 点击不再跳出插件。文档内普通链接不受影响。
            ctx.output_mut(|o| {
                o.commands.retain(|c| match c {
                    egui::OutputCommand::OpenUrl(u) => !crate::otools::is_community_url(&u.url),
                    _ => true,
                });
            });
        }

        fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
            self.app.raw_input_hook(raw);
        }

        fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
            self.app.ui(ui);
        }
    }

    fn query() -> String {
        web_sys::window().and_then(|w| w.location().search().ok()).unwrap_or_default()
    }

    fn has_flag(query: &str, key: &str) -> bool {
        query.trim_start_matches('?').split('&').any(|p| p == key || p.starts_with(&format!("{key}=")))
    }

    /// 宿主服务：在 OTools 里用宿主的对话框与磁盘；否则退回上游网页版的行为。
    fn services(inbox: Inbox, place: PlaceInbox) -> Services {
        if crate::otools::available() {
            Services {
                open_async: Some(Box::new(crate::otools::pick_open)),
                place_async: Some(Box::new(crate::otools::pick_place)),
                download: Some(Box::new(|name: &str, bytes: &[u8]| crate::otools::deliver(name, bytes))),
                inbox: Some(inbox),
                place_inbox: Some(place),
                ..Services::default()
            }
        } else {
            Services {
                download: Some(Box::new(|name: &str, bytes: &[u8]| {
                    download(name, bytes).inspect_err(|e| log::error!("download of {name} failed: {e}"))
                })),
                inbox: Some(inbox),
                place_inbox: Some(place),
                ..Services::default()
            }
        }
    }

    /// 不在 OTools 里时的兜底：浏览器下载（改自上游 `apps/vectorcraft-web` 的同名函数）。
    fn download(path: &str, bytes: &[u8]) -> Result<(), String> {
        let js = |e: wasm_bindgen::JsValue| format!("{e:?}");
        let name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "vectorcraft".into());
        let window = web_sys::window().ok_or("no window")?;
        let document = window.document().ok_or("no document")?;
        let parts = js_sys::Array::of1(&js_sys::Uint8Array::from(bytes));
        let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &web_sys::BlobPropertyBag::new()).map_err(js)?;
        let url = web_sys::Url::create_object_url_with_blob(&blob).map_err(js)?;
        let a: web_sys::HtmlAnchorElement = document.create_element("a").map_err(js)?.dyn_into().map_err(|_| "not an anchor")?;
        a.set_href(&url);
        a.set_download(&name);
        document.body().ok_or("no body")?.append_child(&a).map_err(js)?;
        a.click();
        a.remove();
        Ok(())
    }

    pub fn start() {
        eframe::WebLogger::init(log::LevelFilter::Info).ok();
        let query = query();
        wasm_bindgen_futures::spawn_local(async move {
            let Some(document) = web_sys::window().and_then(|w| w.document()) else { return };
            let Some(canvas) = document.get_element_by_id(CANVAS_ID).and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok()) else {
                log::error!("missing <canvas id=\"{CANVAS_ID}\">");
                return;
            };
            let mut options = eframe::WebOptions::default();
            if has_flag(&query, "webgl")
                && let eframe::egui_wgpu::WgpuSetup::CreateNew(create) = &mut options.wgpu_options.wgpu_setup
            {
                create.instance_descriptor.backends = eframe::wgpu::Backends::GL;
            }
            let result = eframe::WebRunner::new()
                .start(
                    canvas,
                    options,
                    Box::new(move |cc| {
                        let inbox: Inbox = Inbox::default();
                        let place: PlaceInbox = PlaceInbox::default();
                        let mut app = VectorcraftApp::new(Session::new(), services(inbox.clone(), place.clone()));
                        // wasm 读不到系统字体：内嵌 CJK + 阿拉伯 fallback，否则中文/日文/阿拉伯文方框。
                        vectorcraft_ui_egui::theme::set_extra_fallback_fonts(vec![
                            ("otools-cjk".into(), std::sync::Arc::new(eframe::egui::FontData::from_static(include_bytes!("../assets/fonts/otools-cjk-subset.ttf")))),
                            ("otools-arabic".into(), std::sync::Arc::new(eframe::egui::FontData::from_static(include_bytes!("../assets/fonts/otools-arabic-subset.ttf")))),
                        ]);
                        // 第一次打开跟随 OTools 的界面主题（插件 webview 的
                        // prefers-color-scheme 不一定等于宿主主题）。
                        if let Some(brightness) = Brightness::parse(&crate::otools::theme()) {
                            app.ui.brightness = brightness;
                        }
                        crate::otools::install(inbox.clone(), place, cc.egui_ctx.clone());
                        // 系统字体桥：取宿主的字体目录喂进上游字体库（异步，到达后请求重绘）。
                        crate::fontbridge::fetch_catalog();
                        Ok(Box::new(WebShell { app, inbox }))
                    }),
                )
                .await;
            // 监听宿主语言切换（OTools 注入的 `otools-locale-changed` 事件）。
            install_locale_listener();
            if let Some(el) = document.get_element_by_id(LOADING_ID) {
                match result {
                    Ok(()) => el.remove(),
                    Err(e) => el.set_inner_html(&format!("<p>VectorCraft 启动失败：{e:?}</p><p>需要支持 WebGPU 或 WebGL2 的浏览器。</p>")),
                }
            }
        });
    }

    /// 监听宿主语言切换（OTools 注入的 `otools-locale-changed` 事件），置位标志并请求重绘；
    /// 真正把 host locale 写进 interface_language 偏好发生在下一帧 `WebShell::logic`。
    fn install_locale_listener() {
        let Some(window) = web_sys::window() else { return };
        let closure = Closure::wrap(Box::new(|| {
            NEEDS_LANG_SYNC.with(|f| f.set(true));
            if let Some(ctx) = APP_CTX.with(|c| c.borrow().clone()) {
                ctx.request_repaint();
            }
        }) as Box<dyn FnMut()>);
        let _ = window.add_event_listener_with_callback(
            "otools-locale-changed",
            closure.as_ref().unchecked_ref::<js_sys::Function>(),
        );
        closure.forget();
    }

    /// 把宿主的 BCP 47 locale 映射到 VectorCraft 的界面语言代码（共享 i18n 的 `Lang`）。
    /// `lang_from_tag` 会把 `zh-CN` 正确映射成 `zh-hans`，不支持的 locale 回退英文。
    fn host_lang_code(host: &str) -> String {
        vectorcraft_ui_egui::i18n::lang_from_tag(host)
            .map(|l| l.code().to_string())
            .unwrap_or_else(|| "en".to_string())
    }
}

#[cfg(target_arch = "wasm32")]
fn main() {
    web::start();
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!("otools-vectorcraft 只在浏览器里运行：在 shell/ 目录用 `trunk build --release` 构建");
}
