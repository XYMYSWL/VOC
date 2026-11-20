//! VOC 单文件安装程序。
//!
//! 双击后自动完成：
//!   1. 证书没被信任时，弹一次 UAC，用管理员权限把 VOC 的自签名证书放进「受信任的人」（不是根证书）；
//!   2. 以当前用户身份安装 / 升级内嵌的 MSIX；
//!   3. 打开 VOC。
//! 以后升级时证书已被信任，不会再弹 UAC。
//!
//!   VOC-Setup.exe               正常安装
//!   VOC-Setup.exe --update      客户端「一键更新」时调用（界面文字不同，其余一样）
//!   VOC-Setup.exe --trust-cert  内部用：在管理员进程里导入证书
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(windows)]
mod win;

static MSIX: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/voc.msix"));
static CER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/voc.cer"));
const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, PartialEq)]
enum Stage {
    Starting,
    Trust,
    Install(u32),
    Done,
    Failed(String),
}

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().skip(1).collect();
    #[cfg(windows)]
    if args.iter().any(|a| a == "--trust-cert") {
        let code = match win::import_cert(CER) {
            Ok(()) => 0,
            Err(_) => 1,
        };
        std::process::exit(code);
    }
    let update = args.iter().any(|a| a == "--update");

    let viewport = egui::ViewportBuilder::default()
        .with_title(if update { "VOC 更新" } else { "安装 VOC" })
        .with_icon(icon())
        .with_inner_size([440.0, 230.0])
        .with_resizable(false)
        .with_maximize_button(false);
    eframe::run_native(
        "VOC Setup",
        eframe::NativeOptions { viewport, centered: true, ..Default::default() },
        Box::new(move |cc| Ok(Box::new(Setup::new(cc, update)))),
    )
}

struct Setup {
    update: bool,
    stage: Arc<Mutex<Stage>>,
    done_at: Option<Instant>,
    /// 安装完成后在桌面创建快捷方式（全新安装时可取消勾选）
    shortcut: Arc<AtomicBool>,
}

impl Setup {
    fn new(cc: &eframe::CreationContext, update: bool) -> Self {
        fonts(&cc.egui_ctx);
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let s = Self { update, stage: Arc::new(Mutex::new(Stage::Starting)), done_at: None, shortcut: Arc::new(AtomicBool::new(true)) };
        s.start(&cc.egui_ctx);
        s
    }

    fn start(&self, ctx: &egui::Context) {
        *self.stage.lock().unwrap() = Stage::Starting;
        let (st, ctx) = (self.stage.clone(), ctx.clone());
        let (update, shortcut) = (self.update, self.shortcut.clone());
        std::thread::spawn(move || {
            let set = Setter(st, ctx);
            let r = install(&set);
            #[cfg(windows)]
            if r.is_ok() && std::env::var_os("VOC_SETUP_DRYRUN").is_none() {
                desktop_shortcut(update, shortcut.load(Ordering::Relaxed));
            }
            set.set(match r {
                Ok(()) => Stage::Done,
                Err(e) => Stage::Failed(e),
            });
        });
    }
}

/// 后台线程更新界面状态
#[derive(Clone)]
struct Setter(Arc<Mutex<Stage>>, egui::Context);

impl Setter {
    fn set(&self, s: Stage) {
        *self.0.lock().unwrap() = s;
        self.1.request_repaint();
    }
}

#[cfg(windows)]
fn install(set: &Setter) -> Result<(), String> {
    // 演示模式（截图 / 调试界面用）：只走一遍进度，不改动系统
    if std::env::var_os("VOC_SETUP_DRYRUN").is_some() {
        for p in (0..=100).step_by(5) {
            set.set(Stage::Install(p));
            std::thread::sleep(Duration::from_millis(250));
        }
        return Ok(());
    }
    if MSIX.is_empty() || CER.is_empty() {
        return Err("这个安装程序里没有打包 VOC（编译时缺少 MSIX），请到 GitHub Releases 重新下载。".into());
    }
    if !win::cert_trusted(CER)? {
        set.set(Stage::Trust);
        win::trust_cert_elevated()?;
        if !win::cert_trusted(CER)? {
            return Err("证书没有导入成功，请重试。".into());
        }
    }
    set.set(Stage::Install(0));
    let path = std::env::temp_dir().join(format!("VOC-{VERSION}-{}.msix", std::process::id()));
    std::fs::write(&path, MSIX).map_err(|e| format!("写入临时文件失败：{e}"))?;
    let s2 = set.clone();
    let r = win::add_package(&path, move |p| s2.set(Stage::Install(p)));
    let _ = std::fs::remove_file(&path);
    r
}

/// 桌面快捷方式：全新安装按勾选；更新时只在从没处理过的情况下创建一次（用户删掉后不会再自动加回来）
#[cfg(windows)]
fn desktop_shortcut(update: bool, wanted: bool) {
    let marker = std::env::var_os("LOCALAPPDATA").map(|d| std::path::PathBuf::from(d).join("VOC").join("desktop-shortcut"));
    let handled = marker.as_ref().is_some_and(|m| m.exists());
    let create = if update { !handled } else { wanted };
    if create {
        let _ = win::create_desktop_shortcut(); // 失败不影响安装
    }
    if let Some(m) = marker {
        let _ = std::fs::create_dir_all(m.parent().unwrap());
        let _ = std::fs::write(m, if create { "created" } else { "declined" });
    }
}

#[cfg(not(windows))]
fn install(_: &Setter) -> Result<(), String> {
    Err("VOC 安装程序只支持 Windows".into())
}

impl eframe::App for Setup {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let stage = self.stage.lock().unwrap().clone();
        if stage == Stage::Done && self.done_at.is_none() {
            self.done_at = Some(Instant::now());
            #[cfg(windows)]
            if std::env::var_os("VOC_SETUP_DRYRUN").is_none() {
                win::launch_voc();
            }
        }
        if let Some(t) = self.done_at {
            if t.elapsed() > Duration::from_millis(1500) && std::env::var_os("VOC_SETUP_DRYRUN").is_none() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        egui::CentralPanel::default().frame(egui::Frame::new().fill(egui::Color32::from_rgb(24, 25, 30)).inner_margin(egui::Margin::symmetric(22, 10))).show(ui, |ui| {
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("VOC").size(30.0).strong().color(egui::Color32::from_rgb(120, 140, 255)));
                ui.add_space(6.0);
                ui.vertical(|ui| {
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new(format!("版本 {VERSION}")).weak());
                    ui.label(egui::RichText::new("加密语音开黑 · 屏幕共享 · P2P 直连").weak());
                });
            });
            ui.add_space(14.0);
            if !self.update {
                let mut on = self.shortcut.load(Ordering::Relaxed);
                let busy = !matches!(stage, Stage::Done | Stage::Failed(_));
                if ui.add_enabled(busy, egui::Checkbox::new(&mut on, "创建桌面快捷方式")).changed() {
                    self.shortcut.store(on, Ordering::Relaxed);
                }
                ui.add_space(6.0);
            }
            let verb = if self.update { "更新" } else { "安装" };
            match &stage {
                Stage::Starting => {
                    ui.label("准备中…");
                    ui.add(egui::ProgressBar::new(0.0).animate(true));
                }
                Stage::Trust => {
                    ui.label("第一次安装需要信任 VOC 的签名证书，请在弹出的窗口里点「是」。");
                    ui.add(egui::ProgressBar::new(0.0).animate(true));
                }
                Stage::Install(p) => {
                    ui.label(format!("正在{verb}…"));
                    ui.add(egui::ProgressBar::new(*p as f32 / 100.0).show_percentage().animate(true));
                }
                Stage::Done => {
                    ui.label(egui::RichText::new(format!("✔ {verb}完成，正在打开 VOC…")).color(egui::Color32::from_rgb(90, 200, 120)));
                    ui.add(egui::ProgressBar::new(1.0));
                }
                Stage::Failed(e) => {
                    ui.label(egui::RichText::new(format!("{verb}失败")).color(egui::Color32::from_rgb(240, 100, 100)).strong());
                    egui::ScrollArea::vertical().max_height(70.0).show(ui, |ui| {
                        ui.label(e);
                    });
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("重试").clicked() {
                            self.start(&ctx);
                        }
                        if ui.button("关闭").clicked() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                }
            }
        });
    }
}

/// egui 自带字体不含中文
fn fonts(ctx: &egui::Context) {
    let candidates = ["C:\\Windows\\Fonts\\msyh.ttc", "C:\\Windows\\Fonts\\msyh.ttf", "C:\\Windows\\Fonts\\simhei.ttf", "C:\\Windows\\Fonts\\simsun.ttc"];
    let Some(bytes) = candidates.iter().find_map(|p| std::fs::read(p).ok()) else { return };
    let mut f = egui::FontDefinitions::default();
    f.font_data.insert("cjk".into(), Arc::new(egui::FontData::from_owned(bytes)));
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        f.families.entry(family).or_default().push("cjk".into());
    }
    ctx.set_fonts(f);
}

fn icon() -> egui::IconData {
    let bytes: &[u8] = include_bytes!("../../../deploy/windows/Assets/icon-256.png");
    let mut dec = png::Decoder::new(std::io::Cursor::new(bytes));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA);
    let Ok(mut reader) = dec.read_info() else { return egui::IconData::default() };
    let mut buf = vec![0u8; reader.output_buffer_size().unwrap_or(256 * 256 * 4)];
    let Ok(info) = reader.next_frame(&mut buf) else { return egui::IconData::default() };
    buf.truncate(info.buffer_size());
    egui::IconData { rgba: buf, width: info.width, height: info.height }
}
