//! 界面绘制（登录页、主界面、设置、屏幕共享、观看窗口）。逻辑在 `app.rs`，配色和组件在 `theme.rs`。

use super::*;
use crate::icons::Icon;
use crate::theme::{self as th, Tone};
use eframe::egui::{Align, Align2, Color32, CornerRadius, Layout, Margin, RichText, Sense, Stroke, Vec2};

impl App {
    // ============================================================ 登录页

    /// 网络断开、正在自动重连
    fn ui_reconnecting(&mut self, ui: &mut egui::Ui) {
        let Screen::Reconnecting(r) = &self.screen else { return };
        let (reason, attempt, busy, restart) = (r.reason.clone(), r.attempt, r.pending.is_some(), r.server_restart);
        let wait = r.next_at.saturating_duration_since(std::time::Instant::now()).as_secs() + 1;
        let mut cancel = false;
        egui::CentralPanel::default().frame(egui::Frame::new().fill(th::BG_BASE())).show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() * 0.28).max(40.0));
                let title = if restart { "服务器正在重启" } else { "与服务器的连接断开了" };
                ui.label(RichText::new(title).size(22.0).strong().color(th::TEXT()));
                ui.add_space(6.0);
                ui.label(RichText::new(&reason).size(13.0).color(th::TEXT_DIM()));
                ui.add_space(18.0);
                ui.horizontal(|ui| {
                    ui.add_space((ui.available_width() - 220.0).max(0.0) / 2.0);
                    ui.spinner();
                    let text = if busy {
                        format!("正在重连…（第 {} 次）", attempt + 1)
                    } else {
                        format!("{wait} 秒后重连（已失败 {attempt} 次）")
                    };
                    ui.label(RichText::new(text).color(th::TEXT()));
                });
                ui.add_space(8.0);
                ui.label(RichText::new("连上后会自动回到原来的语音频道，聊天记录还在。").size(12.0).color(th::TEXT_FAINT()));
                ui.add_space(16.0);
                if ui.button("取消，回到登录页").clicked() {
                    cancel = true;
                }
            });
        });
        if cancel {
            self.go_login(None);
        }
    }

    fn ui_login(&mut self, ui: &mut egui::Ui, error: Option<String>, connecting: bool) {
        egui::CentralPanel::default().frame(egui::Frame::new().fill(th::BG_BASE())).show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space((ui.available_height() * 0.10).max(28.0));
                    ui.label(RichText::new("VOC").size(46.0).strong().color(th::ACCENT_HOVER()));
                    ui.label(RichText::new("加密语音开黑 · 屏幕共享 · P2P 直连").size(14.0).color(th::TEXT_DIM()));
                    match self.updater.get() {
                        crate::update::UpdateState::Available { version, url, .. } => {
                            ui.add_space(6.0);
                            if self.updater.can_auto_install() {
                                if ui.link(RichText::new(format!("⬆ 发现新版本 v{version}，点此一键更新")).color(th::ACCENT_HOVER())).clicked() {
                                    self.updater.install(ui.ctx().clone());
                                }
                            } else {
                                ui.hyperlink_to(RichText::new(format!("⬆ 发现新版本 v{version}，点此下载")).color(th::ACCENT_HOVER()), url);
                            }
                        }
                        crate::update::UpdateState::Downloading { version, percent } => {
                            ui.add_space(6.0);
                            ui.label(RichText::new(format!("正在下载 v{version}… {percent}%")).color(th::TEXT_DIM()));
                        }
                        _ => {}
                    }
                    ui.add_space(26.0);

                    ui.allocate_ui(Vec2::new(420.0, 0.0), |ui| {
                        ui.set_width(420.0);
                        th::card(ui, |ui| {
                            ui.set_width(380.0);
                            ui.with_layout(Layout::top_down(Align::Min), |ui| {
                            // 已保存的服务器
                            if !self.cfg.servers.is_empty() {
                                th::section(ui, "我的服务器");
                                let mut remove = None;
                                ui.horizontal_wrapped(|ui| {
                                    for (i, sv) in self.cfg.servers.iter().enumerate() {
                                        let active = sv.addr == self.cfg.server && sv.code == self.code_input;
                                        let title = if sv.label.is_empty() { sv.addr.clone() } else { sv.label.clone() };
                                        let btn = egui::Button::new(
                                            RichText::new(title.clone()).color(if active { th::ACCENT_HOVER() } else { th::TEXT() }),
                                        )
                                        .fill(if active { th::ACCENT_SOFT() } else { th::BG_HOVER() })
                                        .stroke(if active { Stroke::new(1.0, th::ACCENT()) } else { Stroke::NONE })
                                        .corner_radius(CornerRadius::same(10));
                                        let r = ui.add(btn).on_hover_text(format!("{} · {}（右键删除）", sv.addr, sv.name));
                                        if r.clicked() {
                                            self.cfg.server = sv.addr.clone();
                                            self.cfg.name = sv.name.clone();
                                            self.code_input = sv.code.clone();
                                            self.server_label = sv.label.clone();
                                        }
                                        r.context_menu(|ui| {
                                            if ui.button("从列表删除").clicked() {
                                                remove = Some(i);
                                                ui.close();
                                            }
                                        });
                                    }
                                });
                                if let Some(i) = remove {
                                    self.cfg.servers.remove(i);
                                }
                                ui.add_space(10.0);
                            }

                            let mut enter = false;
                            let field = |ui: &mut egui::Ui, label: &str| {
                                ui.label(RichText::new(label).size(12.5).color(th::TEXT_DIM()));
                            };
                            field(ui, "服务器地址");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.cfg.server)
                                    .hint_text("IP 或域名，例如 voc.example.com")
                                    .desired_width(f32::INFINITY)
                                    .margin(Margin::symmetric(10, 8)),
                            );
                            ui.add_space(6.0);
                            field(ui, "昵称");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.cfg.name)
                                    .hint_text("你的名字")
                                    .char_limit(32)
                                    .desired_width(f32::INFINITY)
                                    .margin(Margin::symmetric(10, 8)),
                            );
                            ui.add_space(6.0);
                            field(ui, "授权码");
                            ui.horizontal(|ui| {
                                let r = ui.add(
                                    egui::TextEdit::singleline(&mut self.code_input)
                                        .password(!self.show_code)
                                        .hint_text("KH-XXXXXXXX-…（向服务器管理员索取）")
                                        .desired_width(ui.available_width() - 42.0)
                                        .margin(Margin::symmetric(10, 8)),
                                );
                                enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                                let eye = if self.show_code { "🙈" } else { "👁" };
                                if th::glyph_button(ui, eye, "显示 / 隐藏授权码", Tone::Normal).clicked() {
                                    self.show_code = !self.show_code;
                                }
                            });
                            let code_ok = voc_proto::auth::AuthCode::parse(&self.code_input);
                            if !self.code_input.trim().is_empty() {
                                if let Err(e) = &code_ok {
                                    ui.label(RichText::new(e).size(12.0).color(th::RED()));
                                }
                            }
                            ui.checkbox(&mut self.cfg.remember_code, RichText::new("在本机记住授权码").color(th::TEXT_DIM()));
                            ui.add_space(8.0);

                            let can = !connecting && !self.cfg.name.trim().is_empty() && code_ok.is_ok();
                            let label = if connecting { "连接中…" } else { "进入服务器" };
                            if (th::primary_button(ui, label, ui.available_width(), can).clicked() || enter) && can {
                                let ctx = ui.ctx().clone();
                                self.start_connect(&ctx);
                            }
                            if let Some(e) = &error {
                                ui.add_space(6.0);
                                egui::Frame::new()
                                    .fill(th::RED_SOFT())
                                    .corner_radius(CornerRadius::same(8))
                                    .inner_margin(Margin::symmetric(10, 8))
                                    .show(ui, |ui| {
                                        ui.set_width(ui.available_width());
                                        ui.label(RichText::new(e).color(th::RED()));
                                    });
                            }
                            ui.add_space(8.0);
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.server_label)
                                        .hint_text("备注名（可选）")
                                        .desired_width(ui.available_width() - 110.0),
                                );
                                if ui.button("💾 保存服务器").clicked() && !self.cfg.server.trim().is_empty() {
                                    let sv = SavedServer {
                                        label: self.server_label.trim().to_string(),
                                        addr: self.cfg.server.trim().to_string(),
                                        name: self.cfg.name.trim().to_string(),
                                        code: if self.cfg.remember_code { self.code_input.trim().to_string() } else { String::new() },
                                    };
                                    match self.cfg.servers.iter_mut().find(|x| x.addr == sv.addr && x.name == sv.name) {
                                        Some(x) => *x = sv,
                                        None => self.cfg.servers.push(sv),
                                    }
                                }
                            });
                            });
                        });
                    });

                    ui.add_space(14.0);
                    ui.label(
                        RichText::new("🔒 ML-KEM-768 抗量子加密 · 端到端加密 · 流量随机化抗 DPI").size(12.0).color(th::TEXT_FAINT()),
                    );
                    ui.add_space(4.0);
                    if ui.link("🎙 音频设置 / 麦克风测试").clicked() {
                        self.show_settings = true;
                    }
                });
            });
        });
    }

    // ============================================================ 主界面

    fn ui_main(&mut self, ui: &mut egui::Ui) {
        let latency = self.latency_inputs();
        let (jobs_tx, ectx) = (self.image_jobs.0.clone(), self.egui_ctx.clone());
        let Screen::Main(s) = &mut self.screen else { return };
        let me = s.me().cloned();
        let my_channel = me.as_ref().and_then(|u| u.channel_id);
        let mut disconnect = false;
        let mut toggled_state = false;
        // 左下角麦克风 / 耳机菜单里换了设备、要列设备（第 86 项）
        let (mut restart_audio_req, mut dev_refresh_req) = (false, false);
        let mut trust_id: Option<u32> = None;
        let mut watch_toggle: Option<(u32, bool)> = None;
        // 右键「私聊」打开的对话、点 ✕ 关闭的对话、切换到的标签（Some(None) = 频道聊天）
        let mut open_dm: Option<String> = None;
        let mut close_dm: Option<String> = None;
        let mut switch_tab: Option<Option<String>> = None;
        // 手动线路（按昵称）和这次在右键菜单里改的
        let routes_by_name: HashMap<String, voice::PeerRoute> = self.cfg.peer_routes.get(&s.server_key).cloned().unwrap_or_default();
        let mut route_change: Option<(String, voice::PeerRoute)> = None;
        let lnk = self.voice.current_link();
        // 能选来「经他中转」的人：同频道的其他人
        let relay_choices: Vec<String> =
            s.users.iter().filter(|x| x.id != s.user_id && my_channel.is_some() && x.channel_id == my_channel).map(|x| x.name.clone()).collect();
        let mic_silent = self.settings.mic_silent.load(Ordering::Relaxed) && my_channel.is_some();
        // 正在看的私聊：清未读；对话已不存在就回到频道聊天
        if let Some(p) = s.chat_tab.clone() {
            match s.dms.iter_mut().find(|t| t.peer == p) {
                Some(t) => t.unread = 0,
                None => s.chat_tab = None,
            }
        }

        // 直连质量 / 分配 / 加密状态（演示模式用假数据）
        let (direct, alloc_voice, alloc_video) = match self.voice.current_link() {
            Some(l) => {
                let (v, d) = l.allocation();
                (l.direct_peers(), v, d)
            }
            None => (
                self.demo.quality.iter().map(|(id, (q, _, _))| (*id, *q)).collect(),
                self.demo.quality.iter().filter(|(_, x)| x.1).map(|(id, _)| *id).collect(),
                self.demo.quality.iter().filter(|(_, x)| x.2).map(|(id, _)| *id).collect(),
            ),
        };
        // 打洞状态
        let (punch, my_p2p): (HashMap<u32, voice::PunchState>, bool) = match self.voice.current_link() {
            Some(l) => (l.punch_states(), l.p2p_on()),
            None => (self.demo.punch.clone(), true),
        };
        // 成员中继路线（me = 我的用户 id）
        let (relay_routes, me_id) = match self.voice.current_link() {
            Some(l) => (l.relays(), Some(l.my_id())),
            None => (self.demo.relays.clone(), Some(s.user_id)),
        };
        let names: HashMap<u32, String> = s.users.iter().map(|u| (u.id, u.name.clone())).collect();
        let name_of = move |id: u32| names.get(&id).cloned().unwrap_or_else(|| format!("#{id}"));
        let e2e_info: HashMap<u32, (Trust, String, bool)> = match &s.e2e {
            Some(e) => {
                let e = e.lock().unwrap();
                e.members.iter().map(|(id, m)| (*id, (m.trust, m.safety.clone(), e.has_pair(*id)))).collect()
            }
            None => self.demo.e2e.clone(),
        };
        let speaking = |id: u32| -> bool {
            if Some(id) == me.as_ref().map(|u| u.id) {
                self.settings.transmitting.load(Ordering::Relaxed) || self.demo.speaking.contains(&id)
            } else {
                self.voice.is_speaking(id) || self.demo.speaking.contains(&id)
            }
        };

        // 有人的真名刚到（第 60-6 项）：重新套一次名字
        if s.e2e.as_ref().is_some_and(|e| e.lock().unwrap().profiles_dirty) {
            crate::app::apply_real_names(s, self.cfg.name.trim());
        }
        // 进了一个新的语音频道：默认打开语音房间（第 59 项）
        if my_channel != self.room_seen_ch {
            if my_channel.is_some() {
                self.room_view = true;
            }
            self.room_seen_ch = my_channel;
        }
        // 演示：VOC_CHAT=1 显示文字聊天（截图用）
        if self.demo.active && std::env::var_os("VOC_CHAT").is_some() {
            self.room_view = false;
        }
        let mut switch_server: Option<usize> = None;
        // ---------------- 最左：服务器栏（第 59 项：我的服务器，点了切换 = 重新连接）
        egui::Panel::left("rail")
            .frame(egui::Frame::new().fill(th::RAIL()).inner_margin(Margin::symmetric(10, 14)))
            .resizable(false)
            .exact_size(64.0)
            .show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(44.0), Sense::hover());
                    ui.painter().rect_filled(r, CornerRadius::same(12), th::ACCENT());
                    ui.painter().text(r.center(), Align2::CENTER_CENTER, "VOC", egui::FontId::proportional(14.0), Color32::WHITE);
                    ui.add_space(4.0);
                    let (r, _) = ui.allocate_exact_size(Vec2::new(32.0, 2.0), Sense::hover());
                    ui.painter().rect_filled(r, CornerRadius::same(1), th::BORDER());
                    ui.add_space(4.0);
                    let servers: Vec<(String, String, bool)> = self
                        .cfg
                        .servers
                        .iter()
                        .map(|sv| {
                            let label = if sv.label.is_empty() { sv.addr.clone() } else { sv.label.clone() };
                            (label, format!("{} · {}", sv.addr, sv.name), sv.addr == self.cfg.server && sv.name == self.cfg.name)
                        })
                        .collect();
                    let mut any_active = false;
                    for (i, (label, tip, active)) in servers.iter().enumerate() {
                        any_active |= *active;
                        let (r, resp) = ui.allocate_exact_size(Vec2::splat(44.0), Sense::click());
                        let hov = resp.hovered();
                        let p = ui.painter();
                        let radius = if *active || hov { 14 } else { 22 };
                        p.rect_filled(r, CornerRadius::same(radius), if *active { th::ACCENT_SOFT() } else if hov { th::BG_HOVER() } else { th::BG_PANEL() });
                        let ch: String = label.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default();
                        p.text(r.center(), Align2::CENTER_CENTER, ch, egui::FontId::proportional(16.0), if *active { th::ACCENT_HOVER() } else { th::TEXT_DIM() });
                        if *active {
                            let bar = egui::Rect::from_min_size(egui::pos2(r.left() - 10.0, r.top() + 10.0), Vec2::new(4.0, 24.0));
                            p.rect_filled(bar, CornerRadius::same(2), th::ACCENT());
                        }
                        let resp = resp.on_hover_text(if *active { format!("{label}（当前）\n{tip}") } else { format!("切换到 {label}（会断开现在的语音）\n{tip}") });
                        if resp.clicked() && !*active {
                            switch_server = Some(i);
                        }
                        ui.add_space(4.0);
                    }
                    let _ = any_active;
                    let (r, resp) = ui.allocate_exact_size(Vec2::splat(44.0), Sense::click());
                    let p = ui.painter();
                    p.rect_stroke(r, CornerRadius::same(22), Stroke::new(1.0, if resp.hovered() { th::ACCENT() } else { th::BORDER() }), egui::StrokeKind::Inside);
                    crate::icons::paint(p, r.shrink(13.0), Icon::Plus, th::TEXT_DIM());
                    if resp.on_hover_text("添加 / 管理服务器（会断开现在的连接，回到登录页）").clicked() {
                        disconnect = true;
                    }
                });
            });

        // ---------------- 顶栏
        egui::Panel::top("top")
            .frame(egui::Frame::new().fill(th::BG_PANEL()).inner_margin(Margin::symmetric(16, 10)).stroke(Stroke::new(1.0, th::BORDER())))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&s.server_name).size(16.5).strong());
                    if let Some(ch) = my_channel.and_then(|c| s.channels.iter().find(|x| x.id == c)) {
                        ui.label(RichText::new("/").color(th::TEXT_FAINT()));
                        crate::icons::show(ui, Icon::Speaker, 16.0, th::TEXT_DIM());
                        ui.label(RichText::new(&ch.name).color(th::TEXT_DIM()));
                    }
                    ui.add_space(8.0);
                    let transport = self.voice.transport_udp().or(self.demo.transport);
                    match transport {
                        Some(true) => th::pill(ui, "🔒 UDP 直连", th::GREEN(), th::GREEN_SOFT())
                            .on_hover_text("ML-KEM-768 抗量子加密 + 端到端加密，流量随机化"),
                        Some(false) => th::pill(ui, "🔒 TCP 隧道", th::YELLOW(), th::YELLOW_SOFT())
                            .on_hover_text("UDP 不通或被限速，语音暂时走加密 TCP；UDP 恢复后自动切回"),
                        None => th::pill(ui, "🔒 连接中", th::TEXT_DIM(), th::BG_ELEVATED()),
                    };
                    let ping = match self.voice.ping_ms.load(Ordering::Relaxed) {
                        0 => self.demo.ping,
                        p => p,
                    };
                    if ping > 0 {
                        let (fg, bg) = if ping < 80 {
                            (th::GREEN(), th::GREEN_SOFT())
                        } else if ping < 200 {
                            (th::YELLOW(), th::YELLOW_SOFT())
                        } else {
                            (th::RED(), th::RED_SOFT())
                        };
                        th::pill(ui, format!("{ping} ms"), fg, bg).on_hover_text("到服务器的往返延迟");
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        match self.updater.get() {
                            crate::update::UpdateState::Available { version, url, .. } => {
                                let auto = self.updater.can_auto_install();
                                let b = egui::Button::new(RichText::new(format!("⬆ 新版本 v{version}")).size(12.0).color(Color32::WHITE))
                                    .fill(th::ACCENT())
                                    .corner_radius(CornerRadius::same(10));
                                let hint = if auto { "一键更新：下载并安装后自动重新打开（会断开当前语音）" } else { "打开下载页" };
                                if ui.add(b).on_hover_text(hint).clicked() {
                                    if auto {
                                        self.updater.install(ui.ctx().clone());
                                    } else {
                                        ui.ctx().open_url(egui::OpenUrl::new_tab(url));
                                    }
                                }
                            }
                            crate::update::UpdateState::Downloading { percent, .. } => {
                                th::pill(ui, format!("⬇ 更新 {percent}%"), th::TEXT(), th::BG_HOVER());
                            }
                            _ => {}
                        }
                        if ui.button(RichText::new("断开").color(th::TEXT_DIM())).on_hover_text("断开连接").clicked() {
                            disconnect = true;
                        }
                        if th::icon_button(ui, Icon::Settings, "设置", Tone::Normal).clicked() {
                            self.show_settings = !self.show_settings;
                        }
                    });
                });
            });

        // ---------------- 左侧：频道 + 自己
        egui::Panel::left("channels")
            .frame(egui::Frame::new().fill(th::BG_SIDEBAR()).inner_margin(Margin::symmetric(10, 10)))
            .resizable(true)
            .default_size(290.0)
            .min_size(240.0)
            .show(ui, |ui| {
                // 底部（第 59 项）：「语音已连接」卡片（频道、到服务器延迟、离开）+ 自己的名片（闭麦 / 静音 / 设置）
                egui::Panel::bottom("me").frame(egui::Frame::new().inner_margin(Margin::symmetric(0, 6))).show(ui, |ui| {
                    if let Some(c) = my_channel {
                        egui::Frame::new().fill(th::GREEN_SOFT()).corner_radius(CornerRadius::same(12)).inner_margin(Margin::symmetric(12, 8)).show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.vertical(|ui| {
                                    ui.label(RichText::new("语音已连接").strong().color(th::GREEN()));
                                    let ch = s.channels.iter().find(|x| x.id == c).map(|x| x.name.clone()).unwrap_or_default();
                                    let ping = match self.voice.ping_ms.load(Ordering::Relaxed) {
                                        0 => self.demo.ping,
                                        p => p,
                                    };
                                    let t = if ping > 0 { format!("{ch} · 到服务器 {ping} ms") } else { ch };
                                    ui.label(RichText::new(t).size(12.0).color(th::TEXT_DIM()));
                                });
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if th::icon_button(ui, Icon::Leave, "离开语音频道", Tone::Danger).clicked() {
                                        s.send(ClientMsg::LeaveChannel);
                                    }
                                });
                            });
                        });
                        ui.add_space(6.0);
                    }
                    egui::Frame::new().fill(th::BG_BASE()).corner_radius(CornerRadius::same(12)).inner_margin(Margin::symmetric(10, 8)).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let name = me.as_ref().map(|u| u.name.as_str()).unwrap_or("?");
                            let talking = me.as_ref().is_some_and(|u| speaking(u.id));
                            th::avatar(ui, name, 36.0, talking);
                            // 右边留出三个按钮的位置，名字 / 状态太长时截断（第 77 项：以前会压到按钮底下）
                            let text_w = (ui.available_width() - 3.0 * 34.0 - 2.0 * 18.0 - 8.0).max(40.0);
                            ui.vertical(|ui| {
                                ui.set_max_width(text_w);
                                ui.add_space(1.0);
                                ui.add(egui::Label::new(RichText::new(name).strong()).truncate());
                                let status = match (my_channel, &self.sharer) {
                                    (Some(_), Some(sh)) => format!("正在共享 {}", sh.cfg.desc()),
                                    (Some(_), None) if talking => "正在说话".into(),
                                    (Some(_), None) => "在语音频道里".into(),
                                    _ => "未进入语音频道".into(),
                                };
                                // 自己在服务器里的编号（第 60-6 项：服务器日志里只写编号）
                                let status = format!("{status} · #{}", s.user_id);
                                ui.add(egui::Label::new(RichText::new(&status).size(12.0).color(if talking { th::GREEN() } else { th::TEXT_FAINT() })).truncate())
                                    .on_hover_text(status);
                            });
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.spacing_mut().item_spacing.x = 2.0;
                                if th::icon_button(ui, Icon::Settings, "设置", Tone::Normal).clicked() {
                                    self.show_settings = !self.show_settings;
                                }
                                let muted = self.settings.muted.load(Ordering::Relaxed);
                                let deaf = self.settings.deafened.load(Ordering::Relaxed);
                                let (mk, dk) = (self.cfg.mute_key.label(), self.cfg.deafen_key.label());
                                let dt = if deaf { format!("取消静音（{dk}）") } else { format!("静音：听不到别人（{dk}）") };
                                // 耳机旁边的小菜单（第 86 项，像 Discord）：输出设备、所有人的音量
                                ui.menu_button(RichText::new("⌃").size(11.0).color(th::TEXT_DIM()), |ui| {
                                    ui.set_min_width(270.0);
                                    if self.output_devices.is_empty() {
                                        dev_refresh_req = true;
                                    }
                                    ui.label(RichText::new("输出设备").size(12.0).color(th::TEXT_DIM()));
                                    restart_audio_req |= device_combo_w(ui, "pop_out", &mut self.cfg.output_device, &self.output_devices, 250.0);
                                    ui.add_space(6.0);
                                    ui.label(RichText::new("输出音量（所有人）").size(12.0).color(th::TEXT_DIM()));
                                    if th::vol_slider(ui, &mut self.cfg.output_volume, "", 190.0).changed() {
                                        self.settings.output_volume.set(self.cfg.output_volume);
                                    }
                                    ui.separator();
                                    if ui.button("语音设置 →").clicked() {
                                        self.show_settings = true;
                                        self.settings_tab = 0;
                                        ui.close();
                                    }
                                })
                                .response
                                .on_hover_text("输出设备、所有人的音量");
                                if th::icon_button(ui, if deaf { Icon::HeadphonesOff } else { Icon::Headphones }, &dt, if deaf { Tone::Danger } else { Tone::Normal }).clicked() {
                                    self.settings.deafened.store(!deaf, Ordering::Relaxed);
                                    toggled_state = true;
                                }
                                let mt = if muted { format!("取消闭麦（{mk}）") } else { format!("闭麦（{mk}）") };
                                // 麦克风旁边的小菜单：输入设备、麦克风音量（带实时电平）
                                ui.menu_button(RichText::new("⌃").size(11.0).color(th::TEXT_DIM()), |ui| {
                                    ui.set_min_width(270.0);
                                    if self.input_devices.is_empty() {
                                        dev_refresh_req = true;
                                    }
                                    ui.label(RichText::new("输入设备").size(12.0).color(th::TEXT_DIM()));
                                    restart_audio_req |= device_combo_w(ui, "pop_in", &mut self.cfg.input_device, &self.input_devices, 250.0);
                                    ui.add_space(6.0);
                                    ui.label(RichText::new("麦克风音量").size(12.0).color(th::TEXT_DIM()));
                                    if th::vol_slider(ui, &mut self.cfg.input_gain, "", 190.0).changed() {
                                        self.settings.input_gain.set(self.cfg.input_gain);
                                    }
                                    // 说话时动的电平条（白线 = 语音激活的门槛），方便边说边调
                                    level_meter(ui, self.settings.input_level_db.get(), self.cfg.vad_threshold_db, self.settings.transmitting.load(Ordering::Relaxed));
                                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(50));
                                    ui.separator();
                                    if ui.button("语音设置 →").clicked() {
                                        self.show_settings = true;
                                        self.settings_tab = 0;
                                        ui.close();
                                    }
                                })
                                .response
                                .on_hover_text("输入设备、麦克风音量");
                                if th::icon_button(ui, if muted { Icon::MicOff } else { Icon::Mic }, &mt, if muted { Tone::Danger } else { Tone::Normal }).clicked() {
                                    self.settings.muted.store(!muted, Ordering::Relaxed);
                                    toggled_state = true;
                                }
                            });
                        });
                    });
                });

                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        th::section(ui, "语音频道");
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui
                                .add(egui::Button::new(RichText::new("＋").color(th::TEXT_DIM())).frame(false))
                                .on_hover_text("新建语音频道")
                                .clicked()
                            {
                                self.adding_channel = !self.adding_channel;
                            }
                        });
                    });
                    if self.adding_channel {
                        ui.horizontal(|ui| {
                            let r = ui.add(
                                egui::TextEdit::singleline(&mut s.new_channel)
                                    .hint_text("新频道名，回车创建")
                                    .char_limit(32)
                                    .desired_width(ui.available_width()),
                            );
                            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && !s.new_channel.trim().is_empty() {
                                let name = std::mem::take(&mut s.new_channel);
                                s.send(ClientMsg::CreateChannel { name });
                                self.adding_channel = false;
                            }
                        });
                    }

                    for ch in &s.channels {
                        let here = my_channel == Some(ch.id);
                        let members: Vec<&UserInfo> = s.users.iter().filter(|u| u.channel_id == Some(ch.id)).collect();
                        let full = members.len() >= voc_proto::MAX_CHANNEL_USERS && !here;
                        if channel_row(ui, &ch.name, members.len(), here, full).clicked() && !here && !full {
                            s.send(ClientMsg::JoinChannel { channel_id: ch.id });
                        }
                        for u in members {
                            let is_me = u.id == s.user_id;
                            let info = e2e_info.get(&u.id);
                            // 第一行：头像 + 名字 + 闭麦 / 静音 / 身份警告
                            let resp = member_row(ui, u, speaking(u.id), |ui| {
                                if u.deafened {
                                    th::pill(ui, "静音", th::RED(), th::RED_SOFT()).on_hover_text("听不到别人");
                                } else if u.muted {
                                    th::pill(ui, "闭麦", th::RED(), th::RED_SOFT());
                                }
                                if !is_me && matches!(info, Some((Trust::Changed, _, _))) {
                                    ui.label(RichText::new("⚠").color(th::RED()))
                                        .on_hover_text("加密身份和上次不一样，已暂停加密通信。右键核对安全码后信任。");
                                }
                            });
                            let e2e_tip = match info {
                                _ if is_me => "这是你",
                                Some((Trust::Changed, _, _)) => "⚠ 加密身份已变化，暂停加密通信",
                                Some((_, _, true)) => "🔒 端到端加密已建立",
                                _ => "正在建立端到端加密…",
                            };
                            let resp = resp.on_hover_text(e2e_tip);
                            // 第二行：直连质量、屏幕共享
                            let q = if is_me { None } else { direct.get(&u.id) };
                            // 我和他之间的语音经某个成员中转
                            let via = match me_id {
                                Some(me) if !is_me => relay_routes
                                    .iter()
                                    .find(|r| (r.from == u.id && r.to == me) || (r.from == me && r.to == u.id))
                                    .copied(),
                                _ => None,
                            };
                            // 我正在帮几路语音中转
                            let relaying = match me_id {
                                Some(me) if is_me => relay_routes.iter().filter(|r| r.via == me).count(),
                                _ => 0,
                            };
                            // 和同频道的人之间的直连状态（自己不显示）
                            let link_state = if is_me || !here {
                                None
                            } else if let Some(st) = punch.get(&u.id) {
                                Some(th::LinkState::Punch(*st))
                            } else if !my_p2p {
                                Some(th::LinkState::MeOff)
                            } else if !u.p2p {
                                Some(th::LinkState::ThemOff)
                            } else if !u.udp {
                                Some(th::LinkState::ThemTcp)
                            } else {
                                Some(th::LinkState::Preparing)
                            };
                            if link_state.is_some() || u.sharing.is_some() || relaying > 0 || u.music.is_some() {
                                ui.horizontal(|ui| {
                                    ui.add_space(48.0);
                                    ui.spacing_mut().item_spacing.x = 4.0;
                                    let voice_direct = alloc_voice.contains(&u.id);
                                    let pref = if is_me { voice::PeerRoute::default() } else { routes_by_name.get(&u.name).cloned().unwrap_or_default() };
                                    let fallback = !is_me && lnk.as_ref().is_some_and(|l| l.in_server_fallback(u.id));
                                    if pref.hide_ip {
                                        th::pill(ui, "🔒 不透露 IP（手动）", th::TEXT_DIM(), th::BG_ELEVATED())
                                            .on_hover_text("你选了不和他直连、互相不透露 IP：语音、画面都经服务器（语音可以经队友中转）。右键他的名字可以改");
                                    } else if pref.voice == voice::VoiceRoute::Server {
                                        th::pill(ui, "⇄ 语音走服务器（手动）", th::TEXT_DIM(), th::BG_ELEVATED())
                                            .on_hover_text("你选了和他之间的语音走服务器（画面照常）；右键他的名字可以改回自动");
                                    } else if fallback {
                                        th::pill(ui, "⇄ 语音暂走服务器", th::YELLOW(), th::BG_ELEVATED())
                                            .on_hover_text("语音优先：你说话时他没听到（直连 / 中转那条路不通），先改经服务器发给他，一分钟后再试回直连");
                                    } else {
                                        match (via, q) {
                                            (Some(r), q) if !q.is_some_and(|q| voice_direct && !q.unstable) => {
                                                th::relay_chip(ui, &name_of(r.via), r.ms, r.server_ms, q);
                                            }
                                            (_, Some(q)) => th::quality_chip(ui, q, voice_direct, alloc_video.contains(&u.id)),
                                            _ => {
                                                if let Some(st) = link_state {
                                                    th::link_state_chip(ui, st);
                                                }
                                            }
                                        }
                                    }
                                    if !pref.hide_ip && (pref.voice == voice::VoiceRoute::Direct || pref.video == voice::RoutePref::Direct) {
                                        th::pill(ui, "手动直连", th::ACCENT_HOVER(), th::ACCENT_SOFT())
                                            .on_hover_text(format!("你选了：{}。打通了就一直直连；打不通时仍走服务器。右键可以改回自动", pref.label()));
                                    }
                                    if let voice::VoiceRoute::Via(n) = &pref.voice {
                                        if !pref.hide_ip && lnk.as_ref().is_some_and(|l| l.manual_via_unmet(u.id).is_some()) {
                                            th::pill(ui, format!("经 {n} 中转暂不可用"), th::YELLOW(), th::BG_ELEVATED()).on_hover_text(format!(
                                                "你选了语音经 {n} 中转，但现在安排不了：{n} 不在频道、关了「帮队友中转」、和你们俩没打通，\
                                                 或者对方选了走服务器 / 选了别的中转人。冲突时按更保守的，现在先走服务器"
                                            ));
                                        }
                                    }
                                    if relaying > 0 {
                                        let me = me_id.unwrap_or_default();
                                        let mine: Vec<&voc_proto::RelayRoute> = relay_routes.iter().filter(|r| r.via == me).collect();
                                        let lines: Vec<String> = mine
                                            .iter()
                                            .map(|r| {
                                                format!(
                                                    "{} → {}：{} ms（服务器 {} ms，快 {} ms）",
                                                    name_of(r.from),
                                                    name_of(r.to),
                                                    r.ms,
                                                    r.server_ms,
                                                    r.server_ms.saturating_sub(r.ms)
                                                )
                                            })
                                            .collect();
                                        let mut pairs: Vec<(u32, u32)> = mine.iter().map(|r| (r.from.min(r.to), r.from.max(r.to))).collect();
                                        pairs.sort();
                                        pairs.dedup();
                                        let mut people: Vec<u32> = pairs.iter().flat_map(|&(a, b)| [a, b]).collect();
                                        people.sort();
                                        people.dedup();
                                        let label = match pairs.as_slice() {
                                            [(a, b)] => format!("🔁 帮 {} ↔ {} 中转", name_of(*a), name_of(*b)),
                                            _ => format!("🔁 帮 {} 人中转", people.len()),
                                        };
                                        th::pill(ui, label, th::GREEN(), th::GREEN_SOFT()).on_hover_text(format!(
                                            "你和队友的直连又快又稳，服务器安排你帮他们转发语音（约占上行 {:.2} Mbps）：\n{}\n可以在 设置 → 网络 里关闭。",
                                            mine.len() as f64 * voice::VOICE_COST_BPS as f64 / 1e6,
                                            lines.join("\n")
                                        ));
                                    }
                                    if let Some(name) = &u.music {
                                        let tip = if is_me { "你正在放伴奏" } else { "伴奏中。右键名字可以单独调伴奏音量" };
                                        // 程序名是端到端发来的（服务器只知道在放伴奏）；自己的直接用本地的
                                        let real = if is_me {
                                            self.music.as_ref().map(|m| m.name.clone())
                                        } else {
                                            s.e2e.as_ref().and_then(|e| e.lock().unwrap().profiles.get(&u.id).and_then(|p| p.music.clone()))
                                        };
                                        let name = real.unwrap_or_else(|| name.clone());
                                        th::pill(ui, format!("🎵 {name}"), th::ACCENT_HOVER(), th::ACCENT_SOFT()).on_hover_text(tip);
                                    }
                                    if let Some(desc) = &u.sharing {
                                        if is_me {
                                            th::pill(ui, format!("🖥 共享中 {desc}"), th::ACCENT_HOVER(), th::ACCENT_SOFT());
                                        } else if here && video::av1_only(desc) && !video::can_decode_av1() && !self.views.contains_key(&u.id) {
                                            // 共享者强制 AV1、本机解不了：看不了，写原因（第 61 项）
                                            th::pill(ui, format!("🖥 共享中 {desc} · 你看不了"), th::TEXT_DIM(), th::BG_ELEVATED()).on_hover_text(
                                                "共享者设置了「强制 AV1 编码」，你这台电脑解不了 AV1。\n\
                                                 能解 AV1 需要支持 AV1 解码的显卡（NVIDIA RTX 30 系、AMD RX 6000、Intel Arc / 11 代酷睿核显及以后），\n\
                                                 并装好 Windows 的「AV1 视频扩展」。也可以请共享者关掉「强制 AV1」。",
                                            );
                                        } else if here {
                                            let watching = self.views.contains_key(&u.id);
                                            let (txt, fg, bg) = if watching {
                                                ("📺 观看中".to_string(), Color32::WHITE, th::ACCENT())
                                            } else {
                                                (format!("📺 观看 {desc}"), th::ACCENT_HOVER(), th::ACCENT_SOFT())
                                            };
                                            let b = egui::Button::new(RichText::new(txt).size(12.0).color(fg))
                                                .fill(bg)
                                                .corner_radius(CornerRadius::same(10))
                                                .min_size(Vec2::new(0.0, 22.0));
                                            if ui.add(b).on_hover_text(if watching { "关闭画面" } else { "观看屏幕共享" }).clicked() {
                                                watch_toggle = Some((u.id, !watching));
                                            }
                                        } else {
                                            th::pill(ui, "🖥 共享中", th::TEXT_DIM(), th::BG_ELEVATED());
                                        }
                                    }
                                });
                            }
                            if !is_me {
                                resp.context_menu(|ui| {
                                    ui.set_min_width(240.0);
                                    ui.label(RichText::new(&u.name).strong());
                                    if ui.button("💬 私聊").on_hover_text("只有你们两个看得到，端到端加密，关闭标签或关掉 VOC 后清除").clicked() {
                                        open_dm = Some(u.name.clone());
                                        ui.close();
                                    }
                                    {
                                        use voice::{RoutePref as R, VoiceRoute as V};
                                        let cur = routes_by_name.get(&u.name).cloned().unwrap_or_default();
                                        let mut r = cur.clone();
                                        ui.separator();
                                        ui.add_enabled_ui(!r.hide_ip, |ui| {
                                            ui.label(RichText::new("语音线路（两个方向都按这个）").size(12.0).color(th::TEXT_DIM()));
                                            ui.radio_value(&mut r.voice, V::Auto, "自动（推荐）")
                                                .on_hover_text("能直连就直连；不稳、绕远或超出上行预算时自动走服务器或队友中转。\n你说话他却没听到时会自动先改走服务器");
                                            ui.radio_value(&mut r.voice, V::Direct, "强制 P2P 直连")
                                                .on_hover_text("打通了就一直直连，不因丢包、绕远或上行预算切走；打不通时仍走服务器，保证听得到");
                                            ui.radio_value(&mut r.voice, V::Server, "走服务器")
                                                .on_hover_text("语音不直连、也不经队友中转（画面照常）。「我能听到他、他听不到我」时可以先试这个");
                                            let others: Vec<&String> = relay_choices.iter().filter(|n| **n != u.name).collect();
                                            if !others.is_empty() {
                                                ui.horizontal(|ui| {
                                                    let via_now = matches!(r.voice, V::Via(_));
                                                    let label = match &r.voice {
                                                        V::Via(n) => format!("经 {n} 中转"),
                                                        _ => "经某个队友中转…".into(),
                                                    };
                                                    egui::ComboBox::from_id_salt(("via", u.id)).selected_text(RichText::new(label).color(if via_now { th::ACCENT_HOVER() } else { th::TEXT_DIM() })).show_ui(ui, |ui| {
                                                        for n in others {
                                                            ui.selectable_value(&mut r.voice, V::Via(n.clone()), format!("经 {n} 中转"));
                                                        }
                                                    })
                                                    .response
                                                    .on_hover_text("你说给他、他说给你都经这个队友转（他要开着「帮队友中转」、和你们俩都打通）。\n安排不了时走服务器，不会换别人");
                                                });
                                            }
                                            ui.label(RichText::new("画面线路（共享 / 观看）").size(12.0).color(th::TEXT_DIM()));
                                            ui.horizontal(|ui| {
                                                ui.radio_value(&mut r.video, R::Auto, "自动");
                                                ui.radio_value(&mut r.video, R::Direct, "强制直连");
                                                ui.radio_value(&mut r.video, R::Server, "走服务器");
                                            });
                                        });
                                        ui.checkbox(&mut r.hide_ip, "完全不透露 IP（不和他直连）").on_hover_text(
                                            "你们俩互相看不到对方的 IP：不打洞，语音、画面都不直连（语音可以经队友中转）。\n改语音或画面线路只影响那一项，不会断开其他人",
                                        );
                                        if r != cur {
                                            route_change = Some((u.name.clone(), r));
                                        }
                                        ui.separator();
                                    }
                                    {
                                        let mut m = self.mixer.lock().unwrap();
                                        let mut v = m.volume(u.id);
                                        if th::vol_slider(ui, &mut v, "音量", 130.0).changed() {
                                            m.set_volume(u.id, v);
                                        }
                                    }
                                    if u.sharing.is_some() && u.share_audio {
                                        let sid = crate::audio::stream_mixer_id(voc_proto::udp::SHARE_AUDIO, u.id);
                                        let mut m = self.mixer.lock().unwrap();
                                        let mut v = m.volume(sid);
                                        if th::vol_slider(ui, &mut v, "共享声音", 130.0).changed() {
                                            m.set_volume(sid, v);
                                        }
                                    }
                                    if u.music.is_some() {
                                        let mid = crate::audio::stream_mixer_id(voc_proto::udp::MUSIC, u.id);
                                        let mut m = self.mixer.lock().unwrap();
                                        let mut v = m.volume(mid);
                                        if th::vol_slider(ui, &mut v, "伴奏", 130.0).changed() {
                                            m.set_volume(mid, v);
                                        }
                                    }
                                    // 嘴到耳延迟：按实际走的线路估算
                                    let one_way = match (via, direct.get(&u.id)) {
                                        (Some(r), _) => r.ms / 2,
                                        (None, Some(q)) if alloc_voice.contains(&u.id) && !q.unstable => q.rtt_ms / 2,
                                        _ => self.voice.ping_ms.load(Ordering::Relaxed).max(1),
                                    };
                                    let (m2e, parts) = latency.mouth_to_ear(u.id, one_way);
                                    ui.label(RichText::new(format!("听他说话的延迟：约 {m2e} ms")).size(12.5).color(th::TEXT()));
                                    ui.label(RichText::new(parts).size(11.0).color(th::TEXT_FAINT()));
                                    if let Some(r) = via {
                                        ui.label(
                                            RichText::new(th::relay_compare(&name_of(r.via), r.ms, r.server_ms, direct.get(&u.id)))
                                                .size(12.0)
                                                .color(th::TEXT_DIM()),
                                        );
                                    } else if let Some(q) = direct.get(&u.id) {
                                        ui.label(
                                            RichText::new(format!(
                                                "P2P：延迟 {} ms · 丢包 {}% · 抖动 {} ms",
                                                q.rtt_ms, q.loss_pct, q.jitter_ms
                                            ))
                                            .size(12.0)
                                            .color(th::TEXT_DIM()),
                                        );
                                    }
                                    if let Some((trust, code, _)) = info {
                                        ui.separator();
                                        ui.label(RichText::new("对方的安全码").size(12.0).color(th::TEXT_DIM()));
                                        ui.label(RichText::new(code).monospace());
                                        if *trust == Trust::Changed && ui.button("✅ 已核对，信任新身份").clicked() {
                                            trust_id = Some(u.id);
                                            ui.close();
                                        }
                                    }
                                });
                            }
                        }
                    }

                    let idle: Vec<&UserInfo> = s.users.iter().filter(|u| u.channel_id.is_none()).collect();
                    if !idle.is_empty() {
                        ui.add_space(12.0);
                        th::section(ui, &format!("在线 — {}", idle.len()));
                        for u in idle {
                            let resp = member_row(ui, u, false, |_| {});
                            if u.id != s.user_id {
                                resp.context_menu(|ui| {
                                    if ui.button("💬 私聊").clicked() {
                                        open_dm = Some(u.name.clone());
                                        ui.close();
                                    }
                                });
                            }
                        }
                    }
                });
            });

        let mut pick_image = false;
        let mut open_image: Option<egui::TextureHandle> = None;
        // 点了表情回应：(消息, 表情)
        let mut react: Option<((u32, u64), String)> = None;
        let my_id = s.user_id;
        let my_name = s.me().map(|u| u.name.clone()).unwrap_or_default();
        // ---------------- 中间：文字聊天
        egui::CentralPanel::default().frame(egui::Frame::new().fill(th::BG_PANEL()).inner_margin(Margin::symmetric(18, 12))).show(
            ui,
            |ui| {
                let room_on = self.room_view && my_channel.is_some();
                if !s.dms.is_empty() || my_channel.is_some() {
                    egui::Panel::top("chat_tabs").frame(egui::Frame::new().inner_margin(Margin::symmetric(0, 4))).show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            if my_channel.is_some() && th::tab(ui, Icon::Grid, "语音房间", room_on, 0).on_hover_text("频道里的人、屏幕共享和控制栏").clicked() {
                                self.room_view = true;
                            }
                            if th::tab(ui, Icon::Hash, "频道聊天", !room_on && s.chat_tab.is_none(), 0).clicked() {
                                self.room_view = false;
                                switch_tab = Some(None);
                            }
                            for t in &s.dms {
                                let sel = !room_on && s.chat_tab.as_deref() == Some(t.peer.as_str());
                                if th::tab(ui, Icon::Chat, &t.peer, sel, t.unread).clicked() {
                                    self.room_view = false;
                                    switch_tab = Some(Some(t.peer.clone()));
                                }
                                if sel && ui.small_button("✕").on_hover_text("关闭并清除这段私聊（阅后即焚）").clicked() {
                                    close_dm = Some(t.peer.clone());
                                }
                            }
                        });
                    });
                }
                if room_on {
                    // ---------------- 语音房间（第 59 项）：屏幕共享卡片 + 每人一张卡片 + 底部控制栏
                    let my_ch = my_channel.unwrap_or_default();
                    let members: Vec<UserInfo> = s.users.iter().filter(|u| u.channel_id == Some(my_ch)).cloned().collect();
                    egui::Panel::bottom("room_ctl").frame(egui::Frame::new().inner_margin(Margin::symmetric(0, 10))).show(ui, |ui| {
                        let muted = self.settings.muted.load(Ordering::Relaxed);
                        let deaf = self.settings.deafened.load(Ordering::Relaxed);
                        let w = 72.0 * 4.0 + 84.0 + 6.0 * 5.0 + 24.0;
                        ui.horizontal(|ui| {
                            ui.add_space(((ui.available_width() - w) / 2.0).max(0.0));
                            egui::Frame::new()
                                .fill(th::BG_PANEL())
                                .stroke(Stroke::new(1.0, th::BORDER()))
                                .corner_radius(CornerRadius::same(16))
                                .inner_margin(Margin::same(8))
                                .shadow(egui::epaint::Shadow { offset: [0, 4], blur: 16, spread: 0, color: Color32::from_black_alpha(if th::is_light() { 18 } else { 60 }) })
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        ui.spacing_mut().item_spacing.x = 6.0;
                                        if th::tool_button(ui, if muted { Icon::MicOff } else { Icon::Mic }, if muted { "已闭麦" } else { "闭麦" }, muted, false).clicked() {
                                            self.settings.muted.store(!muted, Ordering::Relaxed);
                                            toggled_state = true;
                                        }
                                        if th::tool_button(ui, if deaf { Icon::HeadphonesOff } else { Icon::Headphones }, if deaf { "已静音" } else { "静音" }, deaf, false).clicked() {
                                            self.settings.deafened.store(!deaf, Ordering::Relaxed);
                                            toggled_state = true;
                                        }
                                        let sharing = self.sharer.is_some();
                                        if th::tool_button(ui, Icon::Screen, if sharing { "共享中" } else { "共享屏幕" }, sharing, false).clicked() {
                                            self.show_share = true;
                                        }
                                        if th::tool_button(ui, Icon::Music, "伴奏", self.music.is_some(), false)
                                            .on_hover_text("把某个程序的声音放给频道里的人")
                                            .clicked()
                                        {
                                            self.music_window = !self.music_window;
                                            if self.music_window {
                                                self.music_apps = crate::loopback::audio_apps();
                                            }
                                        }
                                        let (r, _) = ui.allocate_exact_size(Vec2::new(1.0, 36.0), Sense::hover());
                                        ui.painter().rect_filled(r, CornerRadius::ZERO, th::BORDER());
                                        if th::tool_button(ui, Icon::Leave, "离开", false, true).clicked() {
                                            s.send(ClientMsg::LeaveChannel);
                                        }
                                    });
                                });
                        });
                    });
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        let ch_name = s.channels.iter().find(|c| c.id == my_ch).map(|c| c.name.clone()).unwrap_or_default();
                        ui.horizontal(|ui| {
                            crate::icons::show(ui, Icon::Speaker, 18.0, th::TEXT_FAINT());
                            ui.label(RichText::new(&ch_name).size(16.0).strong());
                            ui.label(RichText::new(format!("{} 人", members.len())).color(th::TEXT_FAINT()));
                        });
                        ui.add_space(8.0);
                        // 屏幕共享卡片；正在看、没弹出、没全屏的直接把画面嵌在这里（第 59 项效果图），不再浮一个窗口
                        for u in members.iter().filter(|u| u.sharing.is_some()) {
                            if let Some(va) = self.views.get(&u.id).cloned() {
                                let mut v = va.lock().unwrap();
                                if !v.popped && self.fullscreen != Some(u.id) {
                                    *v.view.repaint_target.lock().unwrap() = None;
                                    refresh_viewer(ui.ctx(), u.id, &mut v);
                                    // 画面按 16:9 算高度（第 73 项：以前先定死外框高度，顶栏里的竖分隔线会把顶栏撑高，画面只剩一小块）
                                    let img_h = ((ui.available_width() - 20.0) * 9.0 / 16.0).min(ui.available_height() * 0.62).clamp(200.0, 760.0);
                                    egui::Frame::new()
                                        .fill(Color32::BLACK)
                                        .stroke(Stroke::new(2.0, th::ACCENT()))
                                        .corner_radius(CornerRadius::same(14))
                                        .inner_margin(Margin::same(8))
                                        .show(ui, |ui| {
                                            ui.set_width(ui.available_width());
                                            viewer_body(ui, u.id, &mut v, &self.voice, &self.mixer, Some(img_h));
                                        });
                                    v.embedded = true;
                                    ui.add_space(10.0);
                                    continue;
                                }
                            }
                            let desc = u.sharing.clone().unwrap_or_default();
                            let is_me = u.id == s.user_id;
                            egui::Frame::new()
                                .fill(th::BG_BASE())
                                .stroke(Stroke::new(2.0, if self.views.contains_key(&u.id) || is_me { th::ACCENT() } else { th::BORDER() }))
                                .corner_radius(CornerRadius::same(14))
                                .inner_margin(Margin::symmetric(16, 12))
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    ui.horizontal(|ui| {
                                        crate::icons::show(ui, Icon::Screen, 22.0, th::ACCENT_HOVER());
                                        ui.vertical(|ui| {
                                            let title = if is_me { "你正在共享屏幕".to_string() } else { format!("{} 的屏幕", u.name) };
                                            ui.label(RichText::new(title).strong());
                                            let audio = if u.share_audio { " · 带声音" } else { "" };
                                            ui.label(RichText::new(format!("{desc}{audio}")).size(12.0).color(th::TEXT_FAINT()));
                                        });
                                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                            if is_me {
                                                if ui.button("详情 / 停止").clicked() {
                                                    self.show_share = true;
                                                }
                                            } else if video::av1_only(&desc) && !video::can_decode_av1() && !self.views.contains_key(&u.id) {
                                                ui.label(RichText::new("你看不了：共享者强制 AV1，这台电脑解不了 AV1").size(12.0).color(th::YELLOW()));
                                            } else {
                                                let watching = self.views.contains_key(&u.id);
                                                if watching {
                                                    if ui.button("关闭画面").clicked() {
                                                        watch_toggle = Some((u.id, false));
                                                    }
                                                    ui.label(RichText::new("观看中").color(th::ACCENT_HOVER()));
                                                } else if th::primary_button(ui, "观看", 88.0, true).clicked() {
                                                    watch_toggle = Some((u.id, true));
                                                }
                                            }
                                        });
                                    });
                                });
                            ui.add_space(10.0);
                        }
                        // 每人一张卡片
                        let any_speaking = members.iter().any(|u| speaking(u.id));
                        let cols = ((ui.available_width() + 12.0) / 182.0).floor().max(1.0) as usize;
                        let tw = ((ui.available_width() - 12.0 * (cols as f32 - 1.0)) / cols as f32).max(150.0);
                        let time = ui.input(|i| i.time);
                        for row in members.chunks(cols) {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 12.0;
                                for u in row {
                                    let is_me = u.id == s.user_id;
                                    let talking = speaking(u.id);
                                    let (route, rc) = if is_me {
                                        let ping = match self.voice.ping_ms.load(Ordering::Relaxed) {
                                            0 => self.demo.ping,
                                            p => p,
                                        };
                                        (format!("到服务器 {ping} ms"), th::TEXT_FAINT())
                                    } else {
                                        let via = me_id.and_then(|me| relay_routes.iter().find(|r| (r.from == u.id && r.to == me) || (r.from == me && r.to == u.id)).copied());
                                        match (via, direct.get(&u.id)) {
                                            (Some(r), _) => (format!("经 {} {} ms", name_of(r.via), r.ms), th::YELLOW()),
                                            (None, Some(q)) if alloc_voice.contains(&u.id) && !q.unstable => (format!("直连 {} ms", q.rtt_ms), th::GREEN()),
                                            _ => ("经服务器".to_string(), th::TEXT_FAINT()),
                                        }
                                    };
                                    let (rect, resp) = ui.allocate_exact_size(Vec2::new(tw, 132.0), Sense::hover());
                                    let p = ui.painter().clone();
                                    let p = &p;
                                    p.rect_filled(rect, CornerRadius::same(14), th::BG_BASE());
                                    p.rect_stroke(rect, CornerRadius::same(14), Stroke::new(2.0, if talking { th::GREEN() } else { th::BORDER() }), egui::StrokeKind::Inside);
                                    let c = rect.center() - Vec2::new(0.0, 12.0);
                                    if talking {
                                        p.circle_filled(c, 33.0, th::GREEN_SOFT());
                                        p.circle_stroke(c, 32.0, Stroke::new(2.0, th::GREEN()));
                                    }
                                    p.circle_filled(c, 28.0, th::name_color(&u.name));
                                    let ini: String = u.name.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default();
                                    p.text(c, Align2::CENTER_CENTER, ini, egui::FontId::proportional(22.0), Color32::WHITE);
                                    // 说话时的声波（4 根小竖条跳动）
                                    if talking {
                                        for k in 0..4 {
                                            let h = 4.0 + 8.0 * (0.5 + 0.5 * ((time * 9.0 + k as f64 * 1.3).sin() as f32));
                                            let x = c.x - 9.0 + k as f32 * 6.0;
                                            let r = egui::Rect::from_min_max(egui::pos2(x, c.y + 46.0 - h), egui::pos2(x + 3.0, c.y + 46.0));
                                            p.rect_filled(r, CornerRadius::same(2), th::GREEN());
                                        }
                                    }
                                    let name = if is_me { format!("{}（我）", u.name) } else { u.name.clone() };
                                    let g = p.layout_no_wrap(name, egui::FontId::proportional(12.5), th::TEXT());
                                    let nr = egui::Rect::from_min_size(rect.left_bottom() + Vec2::new(10.0, -30.0), g.size() + Vec2::new(14.0, 6.0));
                                    p.rect_filled(nr, CornerRadius::same(8), th::BG_PANEL());
                                    p.galley(nr.min + Vec2::new(7.0, 3.0), g, th::TEXT());
                                    // 鼠标放在别人的卡片上：顶上那行换成他的音量滑块，滚轮也能调（第 80 项）
                                    let hover = !is_me && ui.rect_contains_pointer(rect);
                                    if hover {
                                        let mut m = self.mixer.lock().unwrap();
                                        let mut v = m.volume(u.id);
                                        let scroll: f32 = ui.input(|i| {
                                            i.events.iter().filter_map(|e| if let egui::Event::MouseWheel { delta, .. } = e { Some(delta.y) } else { None }).sum()
                                        });
                                        if scroll != 0.0 {
                                            let pos = th::gain_to_pos(v) + if scroll > 0.0 { 0.025 } else { -0.025 };
                                            v = th::pos_to_gain(pos);
                                            if (v - 1.0).abs() < 0.04 {
                                                v = 1.0;
                                            }
                                            m.set_volume(u.id, v);
                                        }
                                        // 滚轮在卡片上用来调音量，不再滚动整个房间
                                        ui.input_mut(|i| i.smooth_scroll_delta = Vec2::ZERO);
                                        let sr = egui::Rect::from_min_max(rect.left_top() + Vec2::new(8.0, 6.0), rect.right_top() + Vec2::new(-8.0, 28.0));
                                        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(sr).layout(Layout::left_to_right(Align::Center)));
                                        if th::vol_slider(&mut child, &mut v, "", (sr.width() - 56.0).max(40.0)).changed() {
                                            m.set_volume(u.id, v);
                                        }
                                    } else {
                                        p.text(rect.right_top() + Vec2::new(-10.0, 10.0), Align2::RIGHT_TOP, route, egui::FontId::proportional(11.0), rc);
                                    }
                                    if u.muted || u.deafened {
                                        let b = egui::Rect::from_center_size(rect.right_bottom() + Vec2::new(-22.0, -22.0), Vec2::splat(24.0));
                                        p.circle_filled(b.center(), 12.0, th::RED_SOFT());
                                        crate::icons::paint(p, b.shrink(5.0), if u.deafened { Icon::HeadphonesOff } else { Icon::MicOff }, th::RED());
                                    }
                                    resp.on_hover_text(if u.deafened {
                                        "静音中（听不到别人）"
                                    } else if u.muted {
                                        "闭麦中"
                                    } else if talking {
                                        "正在说话"
                                    } else {
                                        "鼠标放在卡片上可以调他的音量（拖滑块或滚轮）；右键左侧名单里的名字可以调线路、私聊"
                                    });
                                }
                            });
                            ui.add_space(12.0);
                        }
                        if any_speaking {
                            ui.ctx().request_repaint_after(std::time::Duration::from_millis(60));
                        }
                    });
                    return;
                }
                // 聊天搜索（第 59 项效果图里的搜索框）：只在本机已有的记录里搜（聊天端到端加密、关掉 VOC 就清，不上传、不存盘）
                let search_q = self.chat_search.trim().to_lowercase();
                let search_hits: Vec<usize> = if search_q.is_empty() {
                    Vec::new()
                } else {
                    let lines = match &s.chat_tab {
                        Some(p) => s.dms.iter().find(|t| &t.peer == p).map(|t| &t.lines).unwrap_or(&s.chat),
                        None => &s.chat,
                    };
                    lines
                        .iter()
                        .enumerate()
                        .filter(|(_, l)| match l {
                            ChatLine::User { from, text, .. } => text.to_lowercase().contains(&search_q) || from.to_lowercase().contains(&search_q),
                            ChatLine::System(t) => t.to_lowercase().contains(&search_q),
                            _ => false,
                        })
                        .map(|(i, _)| i)
                        .collect()
                };
                if self.chat_search_hit >= search_hits.len() {
                    self.chat_search_hit = search_hits.len().saturating_sub(1);
                }
                egui::Panel::top("chat_search").frame(egui::Frame::new().inner_margin(Margin::symmetric(0, 4))).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        crate::icons::show(ui, Icon::Search, 16.0, th::TEXT_FAINT());
                        let r = ui.add(egui::TextEdit::singleline(&mut self.chat_search).hint_text("搜索聊天（只在本机的记录里搜）").desired_width(220.0));
                        if r.changed() {
                            self.chat_search_hit = usize::MAX; // 新的搜索：跳到最近的一条
                            self.chat_search_jump = true;
                        }
                        if !search_q.is_empty() {
                            let n = search_hits.len();
                            if n == 0 {
                                ui.label(RichText::new("没找到").size(12.0).color(th::TEXT_FAINT()));
                            } else {
                                if self.chat_search_hit == usize::MAX {
                                    self.chat_search_hit = n - 1;
                                }
                                ui.label(RichText::new(format!("{} / {n}", self.chat_search_hit + 1)).size(12.0).color(th::TEXT_DIM()));
                                if ui.small_button("▲").on_hover_text("上一条").clicked() || (r.has_focus() && ui.input(|i| i.key_pressed(egui::Key::ArrowUp))) {
                                    self.chat_search_hit = self.chat_search_hit.saturating_sub(1);
                                    self.chat_search_jump = true;
                                }
                                if ui.small_button("▼").on_hover_text("下一条").clicked() || (r.has_focus() && ui.input(|i| i.key_pressed(egui::Key::ArrowDown))) {
                                    self.chat_search_hit = (self.chat_search_hit + 1).min(n - 1);
                                    self.chat_search_jump = true;
                                }
                            }
                            if ui.small_button("✕").on_hover_text("清除搜索").clicked() {
                                self.chat_search.clear();
                            }
                        }
                    });
                });
                let search_cur = search_hits.get(self.chat_search_hit).copied();
                let mut jump = std::mem::take(&mut self.chat_search_jump);
                egui::Panel::bottom("chat_input").frame(egui::Frame::new().inner_margin(Margin::symmetric(0, 8))).show(ui, |ui| {
                    if mic_silent {
                        ui.label(
                            RichText::new("⚠ 你的麦克风一直完全没有声音，别人听不到你。请检查 Windows 设置 → 隐私和安全性 → 麦克风（「允许桌面应用访问麦克风」要打开），或在 设置 → 音频 换一个麦克风。")
                                .size(12.0)
                                .color(th::YELLOW()),
                        );
                        ui.add_space(4.0);
                    }
                    if let (None, Some(prefix)) = (&s.chat_tab, crate::app::mention_prefix(&s.chat_input)) {
                        let prefix = prefix.to_lowercase();
                        let names: Vec<String> = s
                            .users
                            .iter()
                            .filter(|u| u.id != my_id && u.name.to_lowercase().starts_with(&prefix))
                            .map(|u| u.name.clone())
                            .take(8)
                            .collect();
                        if !names.is_empty() {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(RichText::new("提及：").size(12.0).color(th::TEXT_DIM()));
                                for n in names {
                                    if ui.small_button(format!("@{n}")).clicked() {
                                        let at = s.chat_input.rfind('@').unwrap_or(s.chat_input.len());
                                        s.chat_input.truncate(at);
                                        s.chat_input.push_str(&format!("@{n} "));
                                    }
                                }
                            });
                            ui.add_space(2.0);
                        }
                    }
                    egui::Frame::new()
                        .fill(th::BG_ELEVATED())
                        .corner_radius(CornerRadius::same(12))
                        .inner_margin(Margin::symmetric(12, 6))
                        .show(ui, |ui| {
                          ui.horizontal(|ui| {
                            if ui
                                .add(egui::Button::new(RichText::new("🖼").size(16.0)).frame(false))
                                .on_hover_text("发送图片（只发给同频道的人，最大 20 MB，端到端加密）\n也可以 Ctrl+V 粘贴截图，或把图片拖进窗口")
                                .clicked()
                            {
                                pick_image = true;
                            }
                            let hint = match &s.chat_tab {
                                Some(p) => format!("私聊 {p}…（只有对方看得到 · 端到端加密）"),
                                None => "发送消息…（回车发送 · @ 提及某人 · 端到端加密）".to_string(),
                            };
                            let r = ui.add(
                                egui::TextEdit::singleline(&mut s.chat_input)
                                    .hint_text(hint)
                                    .frame(egui::Frame::NONE)
                                    .desired_width(f32::INFINITY)
                                    .margin(Margin::symmetric(0, 6)),
                            );
                            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                let text = std::mem::take(&mut s.chat_input);
                                let text = text.trim().chars().take(voc_proto::MAX_CHAT_LEN).collect::<String>();
                                if let (false, Some(peer)) = (text.is_empty(), s.chat_tab.clone()) {
                                    let id = s.id_by_name(&peer);
                                    let sent = match (id, &s.e2e) {
                                        (Some(id), Some(e2e)) => e2e.lock().unwrap().send_dm(id, &text),
                                        _ => None,
                                    };
                                    let me = s.me().map(|u| u.name.clone()).unwrap_or_default();
                                    let t = s.dm(&peer);
                                    if let Some(mid) = sent {
                                        t.lines.push(ChatLine::User {
                                            from: me,
                                            text,
                                            ts: voc_proto::now_millis(),
                                            mine: true,
                                            id: Some((my_id, mid)),
                                            reactions: Vec::new(),
                                        });
                                    } else {
                                        t.lines.push(ChatLine::System(
                                            if id.is_none() { "对方不在线，消息没有发出" } else { "和对方的加密尚未建立（或身份待核对），消息没有发出" }.into(),
                                        ));
                                        s.chat_input = text;
                                    }
                                } else if !text.is_empty() {
                                    let mut mid = None;
                                    if let Some(e2e) = &s.e2e {
                                        let (id, _, missing) = e2e.lock().unwrap().send_chat(&text);
                                        mid = Some((my_id, id));
                                        if !missing.is_empty() {
                                            s.chat.push(ChatLine::System(format!(
                                                "以下成员暂时收不到这条消息（加密尚未建立或身份待核对）：{}",
                                                missing.join("、")
                                            )));
                                        }
                                    }
                                    let me = s.me().map(|u| u.name.clone()).unwrap_or_default();
                                    s.chat.push(ChatLine::User { from: me, text, ts: voc_proto::now_millis(), mine: true, id: mid, reactions: Vec::new() });
                                }
                                r.request_focus();
                            }
                          });
                        });
                });
                egui::ScrollArea::vertical().stick_to_bottom(search_q.is_empty()).auto_shrink([false, false]).show(ui, |ui| {
                    let lines = match &s.chat_tab {
                        Some(p) => s.dms.iter().find(|t| &t.peer == p).map(|t| &t.lines).unwrap_or(&s.chat),
                        None => &s.chat,
                    };
                    if lines.is_empty() {
                        ui.add_space(ui.available_height() * 0.35);
                        ui.vertical_centered(|ui| {
                            ui.label(RichText::new("💬").size(32.0));
                            match &s.chat_tab {
                                Some(p) => {
                                    ui.label(RichText::new(format!("和 {p} 的私聊")).color(th::TEXT_DIM()));
                                    ui.label(
                                        RichText::new("只有你们两个看得到 · 端到端加密 · 关闭标签或关掉 VOC 后清除").size(12.0).color(th::TEXT_FAINT()),
                                    );
                                }
                                None => {
                                    ui.label(RichText::new("还没有消息").color(th::TEXT_DIM()));
                                    ui.label(RichText::new("聊天内容端到端加密，服务器看不到").size(12.0).color(th::TEXT_FAINT()));
                                }
                            }
                        });
                    }
                    let mut prev: Option<(&str, u64)> = None;
                    for (li, line) in lines.iter().enumerate() {
                        let hit = search_hits.contains(&li);
                        let is_cur = search_cur == Some(li);
                        match line {
                            ChatLine::User { from, text, ts, mine, id, reactions } => {
                                // 同一人 5 分钟内的连续消息合并显示
                                let grouped = prev.is_some_and(|(p, t)| p == from && ts.saturating_sub(t) < 300_000);
                                let at_me = !*mine && crate::app::mentions(text, &my_name);
                                // 消息正文：@我 的高亮；能回应的右键出表情
                                let mut body = |ui: &mut egui::Ui, react: &mut Option<((u32, u64), String)>| {
                                    let mut rt = RichText::new(text);
                                    if at_me {
                                        rt = rt.color(th::YELLOW());
                                    }
                                    // 搜到的高亮，当前这条更显眼
                                    if hit {
                                        rt = rt.background_color(if is_cur { th::ACCENT_SOFT() } else { th::YELLOW_SOFT() });
                                    }
                                    let r = ui.add(egui::Label::new(rt).wrap().sense(Sense::click()));
                                    if is_cur && jump {
                                        r.scroll_to_me(Some(Align::Center));
                                        jump = false;
                                    }
                                    if let Some(mid) = id {
                                        let r = r.on_hover_text("右键：表情回应");
                                        r.context_menu(|ui| {
                                            ui.horizontal(|ui| {
                                                for e in voc_proto::e2e::REACTIONS {
                                                    if ui.button(RichText::new(e).size(18.0)).clicked() {
                                                        *react = Some((*mid, e.to_string()));
                                                        ui.close();
                                                    }
                                                }
                                            });
                                        });
                                    }
                                    if !reactions.is_empty() {
                                        ui.horizontal_wrapped(|ui| {
                                            for (e, who) in reactions {
                                                let mine_r = who.contains(&my_id);
                                                let names: Vec<String> = who.iter().map(|w| s.name_of(*w)).collect();
                                                let b = egui::Button::new(RichText::new(format!("{e} {}", who.len())).size(12.5))
                                                    .selected(mine_r)
                                                    .corner_radius(CornerRadius::same(10));
                                                if ui.add(b).on_hover_text(names.join("、")).clicked() {
                                                    if let Some(mid) = id {
                                                        *react = Some((*mid, e.clone()));
                                                    }
                                                }
                                            }
                                        });
                                    }
                                };
                                if grouped {
                                    ui.horizontal(|ui| {
                                        ui.add_space(48.0);
                                        ui.vertical(|ui| body(ui, &mut react));
                                    });
                                } else {
                                    ui.add_space(8.0);
                                    ui.horizontal_top(|ui| {
                                        th::avatar(ui, from, 38.0, false);
                                        ui.add_space(4.0);
                                        ui.vertical(|ui| {
                                            ui.horizontal(|ui| {
                                                let c = if *mine { th::ACCENT_HOVER() } else { th::name_color(from) };
                                                ui.label(RichText::new(from).strong().color(c));
                                                ui.label(RichText::new(th::fmt_time(*ts)).size(11.5).color(th::TEXT_FAINT()));
                                                if at_me {
                                                    th::pill(ui, "@你", th::YELLOW(), th::BG_ELEVATED());
                                                }
                                            });
                                            body(ui, &mut react);
                                        });
                                    });
                                }
                                prev = Some((from.as_str(), *ts));
                            }
                            ChatLine::Image(img) => {
                                ui.add_space(8.0);
                                ui.horizontal_top(|ui| {
                                    th::avatar(ui, &img.from, 38.0, false);
                                    ui.add_space(4.0);
                                    ui.vertical(|ui| {
                                        ui.horizontal(|ui| {
                                            let c = if img.mine { th::ACCENT_HOVER() } else { th::name_color(&img.from) };
                                            ui.label(RichText::new(&img.from).strong().color(c));
                                            ui.label(RichText::new(th::fmt_time(img.ts)).size(11.5).color(th::TEXT_FAINT()));
                                        });
                                        if let Some(tex) = &img.tex {
                                            let r = ui
                                                .add(egui::Image::new(tex).max_size(Vec2::new(360.0, 260.0)).corner_radius(8).sense(Sense::click()))
                                                .on_hover_text("点击查看大图");
                                            if r.clicked() {
                                                open_image = Some(tex.clone());
                                            }
                                        }
                                        let status = if let Some(e) = &img.error {
                                            Some((format!("⚠ {e}"), th::RED()))
                                        } else if let Some((got, total)) = img.receiving {
                                            Some((format!("🖼 接收中 {}%", got * 100 / total.max(1)), th::TEXT_DIM()))
                                        } else if let Some((p, total)) = &img.sending {
                                            let sent = p.load(Ordering::Relaxed);
                                            (sent < *total).then(|| (format!("发送中 {}%", sent * 100 / (*total).max(1)), th::TEXT_DIM()))
                                        } else {
                                            None
                                        };
                                        if let Some((t, c)) = status {
                                            ui.label(RichText::new(t).size(12.0).color(c));
                                        }
                                    });
                                });
                                prev = None;
                            }
                            ChatLine::System(t) => {
                                ui.add_space(6.0);
                                ui.vertical_centered(|ui| {
                                    egui::Frame::new()
                                        .fill(th::BG_ELEVATED())
                                        .corner_radius(CornerRadius::same(10))
                                        .inner_margin(Margin::symmetric(10, 3))
                                        .show(ui, |ui| {
                                            ui.add(egui::Label::new(RichText::new(s.real_names_in(t)).size(12.0).color(th::TEXT_DIM())).wrap());
                                        });
                                });
                                prev = None;
                            }
                        }
                    }
                    ui.add_space(4.0);
                });
            },
        );

        // ---------------- 动作
        if let Some(t) = open_image {
            self.image_view = Some(t);
        }
        if let Some((mid, emoji)) = react {
            let peer = s.chat_tab.clone();
            let lines = s.lines_for(peer.as_deref());
            // 已经点过同一个表情就是取消
            let on = !lines.iter().any(|l| matches!(l, ChatLine::User { id: Some(i), reactions, .. }
                if *i == mid && reactions.iter().any(|(e, w)| *e == emoji && w.contains(&my_id))));
            crate::app::apply_reaction(lines, mid, &emoji, my_id, on);
            let to: Vec<u32> = match &peer {
                Some(p) => s.id_by_name(p).into_iter().collect(),
                None => s.users.iter().map(|u| u.id).filter(|i| *i != my_id).collect(),
            };
            if let Some(e2e) = &s.e2e {
                let r = voc_proto::e2e::React { from: mid.0, id: mid.1, emoji, on, dm: peer.is_some() };
                e2e.lock().unwrap().send_react(&to, &r);
            }
        }
        if let Some(tab) = switch_tab {
            s.chat_tab = tab;
        }
        if let Some((name, r)) = route_change {
            let m = self.cfg.peer_routes.entry(s.server_key.clone()).or_default();
            if r.is_default() {
                m.remove(&name);
            } else {
                m.insert(name, r);
            }
        }
        if let Some(p) = open_dm {
            s.dm(&p);
            s.chat_tab = Some(p);
        }
        if let Some(p) = close_dm {
            if let Some(i) = s.dms.iter().position(|t| t.peer == p) {
                let mut t = s.dms.remove(i);
                crate::app::wipe_dm(&mut t);
            }
            if s.chat_tab.as_deref() == Some(p.as_str()) {
                s.chat_tab = None;
            }
        }
        if pick_image {
            crate::app::queue_image(&jobs_tx, &ectx, || {
                let path = rfd::FileDialog::new()
                    .set_title("选择要发送的图片")
                    .add_filter("图片", &["png", "jpg", "jpeg", "gif", "webp", "bmp"])
                    .pick_file()
                    .ok_or("没有选择图片")?;
                crate::images::prepare_file(&path)
            });
        }
        // Ctrl+V 粘贴截图、把图片拖进窗口
        let (paste, dropped) = ui.input(|i| {
            let paste = i.events.iter().any(|e| {
                matches!(e, egui::Event::Key { key: egui::Key::V, pressed: true, modifiers, .. } if modifiers.command)
            });
            let dropped: Vec<std::path::PathBuf> = i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect();
            (paste, dropped)
        });
        if paste {
            if let Ok(img) = arboard::Clipboard::new().and_then(|mut c| c.get_image()) {
                let (w, h, rgba) = (img.width as u32, img.height as u32, img.bytes.into_owned());
                crate::app::queue_image(&jobs_tx, &ectx, move || crate::images::prepare_rgba(w, h, rgba));
            }
        }
        for path in dropped {
            crate::app::queue_image(&jobs_tx, &ectx, move || crate::images::prepare_file(&path));
        }
        if let Some((id, on)) = watch_toggle {
            // 低带宽模式下默认看低画质层
            let low = on && self.cfg.low_bandwidth;
            s.send(ClientMsg::Watch { target: id, on, low, route: 0, cursor: true });
            if on {
                if let Some(view) = self.voice.video.watch(id, true) {
                    self.views.insert(id, crate::app::Viewer::new_ref(view, low, 0));
                }
            } else {
                self.views.remove(&id);
                self.voice.video.watch(id, false);
            }
        }
        if let (Some(id), Some(e2e)) = (trust_id, &s.e2e) {
            let mut e = e2e.lock().unwrap();
            e.trust(id);
            let name = e.members.get(&id).map(|m| m.name.clone()).unwrap_or_default();
            drop(e);
            s.chat.push(ChatLine::System(format!("已信任 {name} 的新身份，恢复加密通信")));
        }
        if toggled_state {
            self.sync_voice_state();
        }
        if dev_refresh_req {
            self.refresh_devices();
        }
        if restart_audio_req {
            self.restart_audio();
        }
        if disconnect {
            self.go_login(None);
        }
        // 服务器栏切换服务器：断开现在的，用保存的地址 / 昵称 / 授权码重新连接（第 59 项）
        if let Some(sv) = switch_server.and_then(|i| self.cfg.servers.get(i).cloned()) {
            self.go_login(None);
            self.cfg.server = sv.addr.clone();
            self.cfg.name = sv.name.clone();
            self.code_input = sv.code.clone();
            self.server_label = sv.label.clone();
            let ctx = self.egui_ctx.clone();
            self.start_connect(&ctx);
        }
    }

    // ============================================================ 设置

    fn ui_settings(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }
        if self.input_devices.is_empty() && self.output_devices.is_empty() {
            self.refresh_devices();
        }
        let mut open = true;
        let mut restart = false;
        let mut apply_dn = false;
        egui::Window::new(RichText::new("设置").size(17.0).strong())
            .id(egui::Id::unique("settings"))
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            // 第一次打开在正中间，之后可以拖（第 83 项：以前钉在中间拖不动）
            .pivot(Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .constrain(true)
            .show(ctx, |ui| {
                // 左侧分类导航、右侧内容（第 59 项）
                const NAV: [(u8, Icon, &str); 8] = [
                    (0, Icon::Speaker, "音频"),
                    (6, Icon::Keyboard, "按键"),
                    (1, Icon::Wave, "降噪"),
                    (4, Icon::Screen, "视频"),
                    (2, Icon::Wifi, "网络"),
                    (3, Icon::Lock, "加密"),
                    (7, Icon::Palette, "外观"),
                    (5, Icon::Info, "关于"),
                ];
                let h = (ctx.content_rect().height() - 140.0).clamp(320.0, 640.0);
                ui.horizontal_top(|ui| {
                    ui.vertical(|ui| {
                        ui.set_width(140.0);
                        for (id, icon, label) in NAV {
                            if th::tab(ui, icon, label, self.settings_tab == id, 0).clicked() {
                                self.settings_tab = id;
                            }
                            ui.add_space(2.0);
                        }
                    });
                    let (r, _) = ui.allocate_exact_size(Vec2::new(1.0, h), Sense::hover());
                    ui.painter().rect_filled(r, CornerRadius::ZERO, th::BORDER());
                    ui.add_space(10.0);
                    ui.vertical(|ui| {
                    ui.set_width(470.0);
                    let title = NAV.iter().find(|n| n.0 == self.settings_tab).map(|n| n.2).unwrap_or("");
                    ui.label(RichText::new(title).size(20.0).strong());
                    ui.add_space(6.0);
                egui::ScrollArea::vertical().max_height(h - 40.0).auto_shrink([false, false]).show(ui, |ui| {
                    if self.settings_tab == 7 {
                        self.ui_appearance_page(ui);
                    }
                    if self.settings_tab == 0 {
                    th::section(ui, "音频设备");
                    egui::Grid::new("audio").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                        ui.label("麦克风");
                        restart |= device_combo(ui, "in", &mut self.cfg.input_device, &self.input_devices);
                        ui.end_row();
                        ui.label("扬声器");
                        restart |= device_combo(ui, "out", &mut self.cfg.output_device, &self.output_devices);
                        ui.end_row();
                    });
                    if let Some(st) = &self.streams {
                        ui.label(RichText::new(format!("输入：{}\n输出：{}", st.input_desc, st.output_desc)).size(11.5).color(th::TEXT_FAINT()));
                    }
                    for e in &self.audio_errors {
                        ui.label(RichText::new(e).color(th::RED()));
                    }
                    if ui.small_button("🔄 刷新设备").clicked() {
                        self.refresh_devices();
                        restart = true;
                    }

                    ui.add_space(8.0);
                    th::section(ui, "说话方式");
                    ui.horizontal(|ui| {
                        let mode = if self.cfg.ptt_on && self.cfg.ptt_key.is_set() {
                            format!("按键说话（{}）", self.cfg.ptt_key.label())
                        } else {
                            "语音激活".to_string()
                        };
                        ui.label(RichText::new(mode).strong());
                        if ui.small_button("按键设置 →").on_hover_text("说话方式、按键说话的键、闭麦 / 静音等快捷键都在「按键」页").clicked() {
                            self.settings_tab = 6;
                        }
                    });
                    let level = self.settings.input_level_db.get();
                    level_meter(ui, level, self.cfg.vad_threshold_db, self.settings.transmitting.load(Ordering::Relaxed));
                    if !self.cfg.ptt_on {
                        if ui
                            .add(egui::Slider::new(&mut self.cfg.vad_threshold_db, -80.0..=-10.0).suffix(" dB").text("灵敏度阈值"))
                            .changed()
                        {
                            self.settings.vad_threshold_db.set(self.cfg.vad_threshold_db);
                        }
                        ui.label(RichText::new("说话时电平条要超过白线、不说话时低于白线；背景噪音大就把阈值调高。").size(11.5).color(th::TEXT_FAINT()));
                    }
                    // 语音音质（第 68 项）：码率越高越清楚、越费流量（每个人说话时每路都按这个发）
                    ui.horizontal(|ui| {
                        ui.label("语音音质");
                        if th::segmented(ui, &mut self.cfg.voice_kbps, &[(32u32, "32 kbps"), (64, "64 kbps"), (96, "96 kbps"), (128, "128 kbps")]) {
                            crate::audio::VOICE_BITRATE.store(self.cfg.voice_kbps * 1000, Ordering::Relaxed);
                        }
                    });
                    ui.label(
                        RichText::new(if self.cfg.low_bandwidth {
                            "现在开着「低带宽模式」：语音固定 12 kbps，这里的选择不生效。"
                        } else {
                            "码率越高人声越清楚、细节越多，流量也越大（96 kbps 说话时每分钟约 0.7 MB）。网差时可以调低。"
                        })
                        .size(11.5)
                        .color(th::TEXT_FAINT()),
                    );
                    if ui
                        .checkbox(&mut self.cfg.aec, "回声消除（不戴耳机、用扬声器外放时开）")
                        .on_hover_text("外放时，扬声器里队友的声音会被麦克风收进去，队友会听到自己的回声。开着它可以消掉。戴耳机时开着也没关系。")
                        .changed()
                    {
                        self.settings.aec.store(self.cfg.aec, Ordering::Relaxed);
                    }
                    if ui
                        .checkbox(&mut self.cfg.low_latency_device, "声卡低延迟模式（实验）")
                        .on_hover_text("声卡驱动支持时，麦克风和扬声器按约 3 ms 一次处理（普通方式约 10 ms），来回少十几毫秒。\n驱动不支持时自动用普通方式（下方「输入 / 输出」会写明用的是哪种）。")
                        .changed()
                    {
                        crate::audio::LOW_LATENCY_DEVICE.store(self.cfg.low_latency_device, Ordering::Relaxed);
                        restart = true;
                    }
                    ui.checkbox(&mut self.cfg.voice_10ms, "低延迟语音（10 ms 一包）").on_hover_text(
                        "语音少 10 ms 延迟；每条线路多占约 30 kbps 上行。\n频道里有人用旧版本时自动按 20 ms 一包发（他那边才听得顺）",
                    );
                    // 第 80 / 86 项：按人耳感觉走，最高 200%（和左下角麦克风 / 耳机菜单里的是同一个值）
                    if th::vol_slider(ui, &mut self.cfg.input_gain, "麦克风音量", 200.0).changed() {
                        self.settings.input_gain.set(self.cfg.input_gain);
                    }
                    if th::vol_slider(ui, &mut self.cfg.output_volume, "输出音量（所有人）", 200.0).changed() {
                        self.settings.output_volume.set(self.cfg.output_volume);
                    }
                    ui.checkbox(&mut self.cfg.notify, "被 @ 或收到私聊时弹出 Windows 通知（VOC 不在前台时）")
                        .on_hover_text("安装版才能弹通知；绿色版只会让任务栏图标闪烁、托盘图标显示红点");
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.cfg.sfx, "进出频道 / 快捷键提示音");
                        if self.cfg.sfx {
                            let mut v = self.cfg.sfx_volume * 100.0;
                            if ui.add(egui::Slider::new(&mut v, 0.0..=100.0).suffix("%").show_value(false)).changed() {
                                self.cfg.sfx_volume = v / 100.0;
                            }
                            if ui.small_button("▶ 试听").clicked() {
                                let mut m = self.mixer.lock().unwrap();
                                m.set_volume(SFX_ID, self.cfg.sfx_volume);
                                for f in audio::chime(Chime::Join).chunks(voc_proto::FRAME_SAMPLES) {
                                    self.sfx_seq = self.sfx_seq.wrapping_add(1);
                                    m.push(SFX_ID, self.sfx_seq, f);
                                }
                            }
                        }
                    });
                    let mut lb = self.settings.loopback.load(Ordering::Relaxed);
                    if ui.checkbox(&mut lb, "麦克风测试（回放自己的声音，建议戴耳机）").changed() {
                        self.settings.loopback.store(lb, Ordering::Relaxed);
                        if !lb {
                            self.mixer.lock().unwrap().retain_users(|id| id != LOOPBACK_ID);
                        }
                    }

                    }
                    if self.settings_tab == 6 {
                        self.ui_keys_page(ui);
                    }
                    if self.settings_tab == 1 {
                    th::section(ui, "AI 降噪");
                    apply_dn |= th::segmented(
                        ui,
                        &mut self.cfg.denoise,
                        &[
                            (DenoiseMode::Off, "关闭"),
                            (DenoiseMode::Rnnoise2, "RNNoise 新版"),
                            (DenoiseMode::Dfn3, "DeepFilterNet3"),
                            (DenoiseMode::Rnnoise, "RNNoise 旧版"),
                            (DenoiseMode::Plugin, "Krisp / 插件"),
                        ],
                    );
                    // DeepFilterNet3 的降噪强度（第 67 项）；选「暴力」弹个提示
                    if self.cfg.denoise == DenoiseMode::Dfn3 {
                        ui.horizontal(|ui| {
                            ui.label("降噪强度");
                            let opts: Vec<(u8, &str)> = crate::denoise::DFN_LEVELS.iter().enumerate().map(|(i, l)| (i as u8, l.0)).collect();
                            if th::segmented(ui, &mut self.cfg.dfn_level, &opts) {
                                apply_dn = true;
                                if self.cfg.dfn_level as usize == crate::denoise::DFN_LEVELS.len() - 1 {
                                    self.show_violent = true;
                                }
                            }
                        });
                        ui.label(
                            RichText::new("标准：噪音最多压低 12 dB（保留一点环境声，人声最自然）；高：24 dB；极高：不设上限；暴力：不设上限再加后置滤波，残留噪音压得最狠，人声可能发闷一点。比 RNNoise 多占一些 CPU、多约 20 ms 延迟；电脑跑不过来时自动退回 RNNoise 新版。")
                                .size(11.5)
                                .color(th::TEXT_FAINT()),
                        );
                    }
                    if self.cfg.denoise == DenoiseMode::Plugin {
                        egui::Grid::new("plugin").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                            ui.label("插件 DLL");
                            ui.add(egui::TextEdit::singleline(&mut self.cfg.denoise_plugin).hint_text("D:\\krisp\\voc_krisp.dll").desired_width(320.0));
                            ui.end_row();
                            ui.label("模型文件");
                            ui.add(egui::TextEdit::singleline(&mut self.cfg.denoise_model).hint_text("Krisp 模型文件路径").desired_width(320.0));
                            ui.end_row();
                        });
                        apply_dn |= ui.button("加载插件").clicked();
                    }
                    // 降噪器的名字随时读（第 82 项：DeepFilterNet3 加载好以前是「加载中」，以前这里停在选它那一刻）；
                    // 编码线程正拿着锁时用上次的
                    if let (Ok(_), Ok(slot)) = (&self.denoise_status, self.denoise.try_lock()) {
                        if let Some(d) = slot.as_ref() {
                            self.denoise_status = Ok(format!("已启用：{}", d.name()));
                        }
                    }
                    match &self.denoise_status {
                        Ok(st) => ui.label(RichText::new(st).size(12.0).color(th::GREEN())),
                        Err(e) => ui.label(RichText::new(e).size(12.0).color(th::RED())),
                    };

                    ui.label(
                        RichText::new("RNNoise 免费内置，对风扇、空调、键盘声效果好；Krisp 需要向 Krisp 申请 SDK 授权后编译插件。")
                            .size(11.5)
                            .color(th::TEXT_FAINT()),
                    );
                    }
                    if self.settings_tab == 2 {
                    self.ui_speedtest(ui);
                    th::section(ui, "网络");
                    if ui
                        .checkbox(&mut self.cfg.low_bandwidth, "低带宽模式（手机热点、流量少或很卡的网络）")
                        .on_hover_text("语音 24→12 kbps、伴奏和共享声音 64→32 kbps、屏幕共享最高 480P 30 帧、看别人共享时默认低画质。\n语音每人每小时约 5 MB。")
                        .changed()
                    {
                        crate::audio::LOW_BANDWIDTH.store(self.cfg.low_bandwidth, Ordering::Relaxed);
                    }
                    if ui
                        .checkbox(&mut self.cfg.p2p, "P2P 直连（省服务器带宽、延迟更低）")
                        .on_hover_text("能直连的队友之间语音不经过服务器。代价：同频道开启了 P2P 的成员能拿到你的公网 IP（局域网地址只给同一个局域网的人）。
关掉 = 不暴露 IP，所有声音和画面都经服务器。也可以右键某个人选「走服务器」，只对他隐藏。")
                        .changed()
                    {
                        if let Some(l) = self.voice.current_link() {
                            l.set_p2p(self.cfg.p2p);
                        }
                    }
                    if self.cfg.p2p {
                        if ui
                            .add(egui::Slider::new(&mut self.cfg.p2p_budget_mbps, 1..=100).suffix(" Mbps").text("直连上行预算"))
                            .on_hover_text("直连要给每个人各发一份。语音优先（约 0.06 Mbps/人），剩下的给屏幕共享画面；超出预算的人走服务器中转。")
                            .changed()
                        {
                            if let Some(l) = self.voice.current_link() {
                                l.set_budget(self.cfg.p2p_budget_mbps * 1_000_000);
                            }
                        }
                        if ui
                            .checkbox(&mut self.cfg.relay, "帮队友中转语音（成员中继）")
                            .on_hover_text(
                                "两个队友之间打不通直连时，服务器会挑一个和双方都直连良好、延迟最低的人帮忙转发语音，比服务器中转更快。\n\
                                 每路约占 0.06 Mbps 上行，只用直连上行预算里剩下的部分，最多 8 路。转发的是端到端加密后的包。",
                            )
                            .changed()
                        {
                            if let Some(l) = self.voice.current_link() {
                                l.set_relay_enabled(self.cfg.relay);
                            }
                        }
                        ui.label(RichText::new(format!("本次流量（从登录起）：{}", crate::traffic::summary())).size(12.0).color(th::TEXT_DIM()))
                            .on_hover_text("按量计费的校园网、手机流量可以参考。按收发的包计（含包头估算），不额外发包；退出重新登录后从 0 算");
                        ui.horizontal(|ui| {
                            let n = crate::pathcache::count();
                            ui.label(RichText::new(format!("记住的直连地址：{n} 位队友")).size(12.0).color(th::TEXT_DIM())).on_hover_text(
                                "和队友打通直连后，把用过的地址加密存在本机（Windows 凭据保护，只有你的 Windows 账户能解开），\n\
                                 下次上线先试这些地址，打洞更快。按队友的身份指纹记，30 天没用自动删。",
                            );
                            if n > 0 && ui.small_button("清除").clicked() {
                                crate::pathcache::clear();
                            }
                        });
                        let summary = match self.voice.current_link() {
                            Some(l) => {
                                let (v, d) = l.allocation();
                                let all = l.direct_peers();
                                let bad = all.values().filter(|q| q.unstable).count();
                                let me = l.my_id();
                                let routes = l.relays();
                                let mut helped: Vec<u32> = routes.iter().filter(|r| r.via == me).flat_map(|r| [r.from, r.to]).collect();
                                helped.sort();
                                helped.dedup();
                                let relaying = helped.len();
                                let via_others = routes.iter().filter(|r| r.to == me).count();
                                format!(
                                    "已打通 {} 人 · 语音直连 {} 人 · 画面直连 {} 人 · 不稳改中转 {} 人 · 经队友中继收听 {} 人 · 我在帮 {} 人中转",
                                    all.len(),
                                    v.len(),
                                    d.len(),
                                    bad,
                                    via_others,
                                    relaying
                                )
                            }
                            None => "未连接".into(),
                        };
                        ui.label(RichText::new(summary).size(12.0).color(th::TEXT_DIM()));
                        ui.label(
                            RichText::new("⚠ 会向同频道成员暴露你的 IP。直连丢包 ≥15%、延迟 >350 ms 或抖动 >80 ms 时自动改走服务器中转。")
                                .size(11.5)
                                .color(th::TEXT_FAINT()),
                        );
                    }

                    ui.add_space(8.0);
                    th::section(ui, "网络自检");
                    match self.voice.current_link() {
                        Some(l) => {
                            let rep = crate::netcheck::analyze(&l.netcheck_snapshot());
                            egui::Grid::new("netcheck").num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
                                for it in &rep.items {
                                    use crate::netcheck::Level;
                                    let (icon, color) = match it.level {
                                        Level::Good => ("✔", th::GREEN()),
                                        Level::Warn => ("⚠", th::YELLOW()),
                                        Level::Bad => ("✖", th::RED()),
                                        Level::Pending => ("…", th::TEXT_FAINT()),
                                        Level::Info => ("•", th::TEXT_DIM()),
                                    };
                                    ui.label(RichText::new(format!("{icon} {}", it.label)).color(color));
                                    ui.label(RichText::new(&it.value).size(12.5));
                                    ui.end_row();
                                }
                            });
                            let (now, avg, peak) = l.upload_kbps();
                            ui.label(
                                RichText::new(format!("VOC 上行：当前 {now} kbps · 30 秒平均 {avg} · 峰值 {peak}（丢包多时先看是不是共享屏幕或别的程序占满了上传）"))
                                    .size(12.0)
                                    .color(th::TEXT_DIM()),
                            );
                            ui.add_space(4.0);
                            for a in &rep.advice {
                                ui.label(RichText::new(format!("👉 {a}")).size(12.0).color(th::TEXT_DIM()));
                            }
                            ui.label(
                                RichText::new("只用连接时已有的测量，不额外发包；这里显示的 IP 只有你自己看得到。")
                                    .size(11.5)
                                    .color(th::TEXT_FAINT()),
                            );
                            ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
                        }
                        None => {
                            ui.label(RichText::new("登录服务器后显示").size(12.0).color(th::TEXT_DIM()));
                        }
                    }
                    }
                    if self.settings_tab == 5 {
                    th::section(ui, "VOC");
                    ui.label(format!("版本 {}", crate::update::VERSION));
                    ui.hyperlink_to(
                        format!("github.com/{}", crate::update::REPO),
                        format!("https://github.com/{}", crate::update::REPO),
                    );
                    ui.label(RichText::new("开源协议：MIT").size(12.0).color(th::TEXT_DIM()));
                    ui.add_space(8.0);
                    th::section(ui, "启动与托盘");
                    ui.checkbox(&mut self.cfg.close_to_tray, "关闭窗口时缩到托盘（语音不断）")
                        .on_hover_text("点右上角 ✕ 后 VOC 留在右下角托盘里继续语音；右键托盘图标可以闭麦、静音或退出。");
                    if self.tray.is_none() && !self.demo.active {
                        ui.label(RichText::new("托盘图标没能创建，关闭窗口会直接退出。").size(11.5).color(th::YELLOW()));
                    }
                    let mut on = self.autostart == crate::autostart::AutoStart::On;
                    let blocked = self.autostart == crate::autostart::AutoStart::BlockedByUser;
                    if ui
                        .add_enabled(!blocked, egui::Checkbox::new(&mut on, "开机自动启动"))
                        .on_hover_text("开机后自动登录上次的服务器、回到上次的语音频道，窗口缩在托盘里。需要记住授权码。")
                        .changed()
                    {
                        self.autostart = crate::autostart::set(on);
                    }
                    match &self.autostart {
                        crate::autostart::AutoStart::BlockedByUser => {
                            ui.label(RichText::new("已在任务管理器「启动应用」里被禁用，请在那里重新启用。").size(11.5).color(th::YELLOW()));
                        }
                        crate::autostart::AutoStart::Unsupported(e) => {
                            ui.label(RichText::new(format!("无法设置开机自启：{e}")).size(11.5).color(th::RED()));
                        }
                        _ => {}
                    }
                    ui.add_space(8.0);
                    th::section(ui, "诊断");
                    ui.horizontal(|ui| {
                        if ui.button("📋 复制诊断信息").on_hover_text("复制到剪贴板，可以直接粘贴发给帮你排查问题的人").clicked() {
                            ctx.copy_text(self.diagnostics());
                            self.diag_copied = Some(std::time::Instant::now());
                        }
                        if self.diag_copied.is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(3)) {
                            ui.label(RichText::new("已复制").color(th::GREEN()));
                        }
                    });
                    ui.checkbox(&mut self.diag_names, "附上编号 ↔ 昵称对照")
                        .on_hover_text("诊断信息和运行日志里队友都写成编号（例如 #12），不写昵称。\n勾上后会在诊断信息末尾附一份「编号是谁」的对照，方便对方看懂；不勾就不带。");
                    ui.label(
                        RichText::new("包含版本、系统、网络质量、声卡和编解码器信息、最近的运行日志；不含聊天、密钥、授权码、服务器地址和任何人的 IP，队友只写编号。")
                            .size(11.5)
                            .color(th::TEXT_FAINT()),
                    );
                    match crate::applog::path() {
                        Some(p) => {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(RichText::new("运行日志：").size(12.0).color(th::TEXT_DIM()));
                                ui.label(RichText::new(p.display().to_string()).size(12.0).monospace());
                            });
                            ui.horizontal(|ui| {
                                if ui.button("📂 打开日志文件夹").on_hover_text("在资源管理器里打开，选中日志文件").clicked() {
                                    let _ = std::process::Command::new("explorer").arg(format!("/select,{}", p.display())).spawn();
                                }
                                if ui.button("📋 复制日志路径").clicked() {
                                    ctx.copy_text(p.display().to_string());
                                    self.log_path_copied = Some(std::time::Instant::now());
                                }
                                if self.log_path_copied.is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(3)) {
                                    ui.label(RichText::new("已复制").color(th::GREEN()));
                                }
                            });
                        }
                        None => {
                            ui.label(RichText::new("运行日志没有开（设置了 VOC_NO_LOG，或者没有可写的位置）").size(12.0).color(th::TEXT_DIM()));
                        }
                    }
                    ui.add_space(8.0);
                    th::section(ui, "更新");
                    ui.checkbox(&mut self.cfg.auto_update, "启动时自动检查更新");
                    if ui
                        .checkbox(&mut self.cfg.update_accel, "中国大陆加速更新")
                        .on_hover_text("打开后一键更新优先从国内的 GitHub 加速镜像下载（一个不行自动换下一个），在中国大陆更快。\n不打开也会在连不上 GitHub 时自动改走镜像。\n版本号和 SHA-256 仍从 GitHub 官方取，下载的文件对不上就丢掉，镜像改不了安装包。")
                        .changed()
                    {
                        self.updater.accel.store(self.cfg.update_accel, Ordering::Relaxed);
                    }
                    ui.horizontal(|ui| {
                        if ui.button("🔄 检查更新").clicked() {
                            self.updater.check(ctx.clone());
                        }
                        match self.updater.get() {
                            crate::update::UpdateState::Idle => {}
                            crate::update::UpdateState::Checking => {
                                ui.spinner();
                            }
                            crate::update::UpdateState::UpToDate => {
                                ui.label(RichText::new("已是最新版本").color(th::GREEN()));
                            }
                            crate::update::UpdateState::Available { version, url, .. } => {
                                if self.updater.can_auto_install() {
                                    if ui.button(RichText::new(format!("⬆ 一键更新到 v{version}")).color(th::ACCENT_HOVER())).clicked() {
                                        self.updater.install(ctx.clone());
                                    }
                                } else {
                                    ui.hyperlink_to(RichText::new(format!("发现新版本 v{version}，点此下载")).color(th::ACCENT_HOVER()), url);
                                }
                                let asset = if crate::update::is_packaged() { crate::update::SETUP_ASSET } else { crate::update::PORTABLE_ASSET };
                                ui.hyperlink_to(RichText::new("国内加速下载").size(12.0), crate::update::accel_link(asset))
                                    .on_hover_text("经国内 GitHub 加速镜像下载（第三方镜像，手动下载时不会自动核对 SHA-256）");
                            }
                            crate::update::UpdateState::Downloading { percent, .. } => {
                                ui.add(egui::ProgressBar::new(percent as f32 / 100.0).desired_width(160.0).show_percentage());
                            }
                            crate::update::UpdateState::Launched => {
                                ui.label("正在启动安装程序…");
                            }
                            crate::update::UpdateState::Failed(e) => {
                                ui.label(RichText::new(e).size(12.0).color(th::RED()));
                                ui.label(RichText::new("GitHub 和国内镜像都没连上，可以稍后再试").size(11.5).color(th::TEXT_DIM()));
                            }
                        }
                    });
                    ui.label(
                        RichText::new("检查更新只访问 GitHub 公开的版本信息，不上传任何本机数据。安装版可以一键更新（自动下载、校验 SHA-256、安装后重新打开）；绿色版请下载新的 zip。设置和身份密钥都会保留。")
                            .size(11.5)
                            .color(th::TEXT_FAINT()),
                    );
                    }
                    if self.settings_tab == 4 {
                    th::section(ui, "观看");
                    ui.checkbox(&mut self.cfg.view_diag_log, "记录观看诊断（看共享时每秒写一行进日志）").on_hover_text(
                        "看共享卡顿、掉帧时打开：每秒把收到 / 解码 / 显示的帧数、丢包、冗余和补发救回多少、往返时间写进日志（设置 → 关于 → 打开日志文件夹）。
                         只记数字，不记画面内容；排查完可以关掉。",
                    );
                    ui.add_space(6.0);
                    th::section(ui, "屏幕共享编码器");
                    let choices = video::encoder_choices();
                    let cur = choices.iter().find(|c| c.0 == self.cfg.share.encoder).map(|c| c.1.clone()).unwrap_or_else(|| choices[0].1.clone());
                    egui::ComboBox::from_id_salt("encoder").width(ui.available_width()).selected_text(cur).show_ui(ui, |ui| {
                        for (k, label) in &choices {
                            ui.selectable_value(&mut self.cfg.share.encoder, k.clone(), label);
                        }
                    });
                    ui.label(
                        RichText::new("自动：按 AV1 > HEVC > H.264 挑所有观看者都能解码的最省带宽格式，优先用显卡硬件编码；AV1 同画质只要 H.264 约 60% 的码率。指定显卡后只用那块显卡；硬件编码出错会自动改用 CPU。")
                            .size(11.5)
                            .color(th::TEXT_FAINT()),
                    );
                    ui.add_space(8.0);
                    th::section(ui, "观看时的解码方式");
                    if th::segmented(ui, &mut self.cfg.hw_decode, &[(false, "软解（CPU）"), (true, "硬解（显卡）")]) {
                        video::HW_DECODE.store(self.cfg.hw_decode, Ordering::Relaxed);
                    }
                    let can: Vec<&str> = video::local_decoders().iter().map(|c| c.label()).collect();
                    ui.label(RichText::new(format!("本机可解码：{}", can.join(" · "))).size(12.0).color(th::TEXT_DIM()));
                    ui.label(
                        RichText::new("对 H.264 / HEVC 生效：软解延迟更低、兼容性最好；硬解省 CPU（适合同时开游戏）。AV1 总是用显卡硬解（显卡不支持时自动改用 HEVC / H.264）。HEVC / AV1 需要系统装有「HEVC 视频扩展」「AV1 视频扩展」。")
                            .size(11.5)
                            .color(th::TEXT_FAINT()),
                    );
                    }
                    if self.settings_tab == 3 {
                    th::section(ui, "隐私");
                    ui.checkbox(&mut self.cfg.hide_name, "对服务器隐藏昵称（重新连接后生效）").on_hover_text(
                        "打开：服务器只知道你是「成员-XXXX」（日志、状态里也只写编号），真名只经端到端加密发给服务器上的其他人。
                         旧版本 VOC 的人看你只能看到「成员-XXXX」。关掉：和以前一样，服务器能看到昵称。",
                    );
                    if let Screen::Main(s) = &self.screen {
                        ui.label(RichText::new(format!("你在这台服务器上是 #{}（服务器日志里只写编号）", s.user_id)).size(11.5).color(th::TEXT_FAINT()));
                    }
                    ui.add_space(6.0);
                    th::section(ui, "端到端加密");
                    ui.label(RichText::new("我的安全码").size(12.0).color(th::TEXT_DIM()));
                    let code = voc_proto::e2e::safety_code(&Identity::from_seed(&self.identity_seed).public);
                    egui::Frame::new().fill(th::BG_BASE()).corner_radius(CornerRadius::same(8)).inner_margin(Margin::symmetric(10, 6)).show(
                        ui,
                        |ui| {
                            ui.label(RichText::new(code).monospace().size(15.0));
                        },
                    );
                    ui.label(
                        RichText::new("别人核对你的身份时，把这串数字念给对方。它来自你本机的身份密钥，换电脑或重装后会变。")
                            .size(11.5)
                            .color(th::TEXT_FAINT()),
                    );
                    }
                });
                    });
                });
            });
        if restart {
            self.restart_audio();
        }
        if apply_dn {
            self.apply_denoise();
        }
        if !open {
            self.show_settings = false;
            self.settings.loopback.store(false, Ordering::Relaxed);
        }
        // 选了「暴力」档（第 67 项）
        if self.show_violent {
            egui::Window::new(RichText::new("降噪强度").strong())
                .id(egui::Id::unique("violent"))
                .collapsible(false)
                .resizable(false)
                .order(egui::Order::Foreground)
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.set_min_width(240.0);
                    ui.vertical_centered(|ui| {
                        ui.add_space(6.0);
                        ui.label(RichText::new("收到！暴力了嗷").size(20.0).strong());
                        ui.add_space(10.0);
                        if th::primary_button(ui, "好", 120.0, true).clicked() {
                            self.show_violent = false;
                        }
                    });
                });
        }
        if self.show_settings {
            self.ui_key_edit(ctx);
        } else if self.key_edit.is_some() {
            self.end_key_edit(None);
        }
    }

    // ============================================================ 测上行带宽（第 71 项）

    fn ui_speedtest(&mut self, ui: &mut egui::Ui) {
        th::section(ui, "上行带宽");
        let running = self.speed.lock().unwrap().running;
        let link = self.voice.current_link();
        let server_ok = matches!(&self.screen, Screen::Main(s) if s.features.iter().any(|f| f == voc_proto::FEATURE_SPEEDTEST));
        ui.horizontal(|ui| {
            let b = ui.add_enabled(!running, egui::Button::new(if running { "正在测…（约 20 秒）" } else { "测上行带宽" }));
            if b.on_hover_text("依次测：你的 VOC 服务器、Cloudflare、Leaseweb 香港（iperf3）。测速网站会看到你的 IP；测的时候语音可能卡一下").clicked() {
                let server = match (&link, server_ok) {
                    (Some(l), true) => {
                        let (l1, l2) = (l.clone(), l.clone());
                        let c: Box<dyn Fn(bool) + Send> = Box::new(move |s| l1.speed_control(s));
                        let p: Box<dyn Fn(usize) + Send> = Box::new(move |n| l2.speed_probe(n));
                        Some((l.server_rtt_ms(), c, p))
                    }
                    _ => None,
                };
                crate::speedtest::spawn(self.speed.clone(), ui.ctx().clone(), server);
            }
            if running {
                ui.spinner();
            }
        });
        ui.label(RichText::new("只在你点按钮时测。会连：你的 VOC 服务器、speed.cloudflare.com、Leaseweb 香港测速节点（它们会看到你的 IP）。").size(11.5).color(th::TEXT_FAINT()));
        let (rows, chosen, fresh) = {
            let s = self.speed.lock().unwrap();
            (s.rows.clone(), s.chosen, s.fresh)
        };
        if !rows.is_empty() {
            egui::Grid::new("speed").num_columns(4).spacing([14.0, 4.0]).striped(true).show(ui, |ui| {
                for h in ["测速服务器", "延迟", "上行", ""] {
                    ui.label(RichText::new(h).size(11.5).color(th::TEXT_FAINT()));
                }
                ui.end_row();
                for r in &rows {
                    ui.label(&r.name);
                    ui.label(r.latency_ms.map(|l| format!("{l} ms")).unwrap_or_else(|| "—".into()));
                    let m = r.mbps.map(|m| format!("{m:.1} Mbps")).unwrap_or_else(|| "—".into());
                    ui.label(if r.dropped { RichText::new(m).strikethrough().color(th::TEXT_FAINT()) } else { RichText::new(m).strong() });
                    let note = if r.dropped { "比其他的低太多，没算进平均".to_string() } else { r.note.clone() };
                    ui.label(RichText::new(note).size(11.5).color(th::TEXT_FAINT()));
                    ui.end_row();
                }
            });
        }
        if let Some(avg) = chosen {
            // 自动选择：直连上行预算设成平均值的八成（留点余量给语音和别的程序）
            let budget = ((avg * 0.8).round() as u32).clamp(1, 100);
            if fresh {
                self.speed.lock().unwrap().fresh = false;
                self.cfg.p2p_budget_mbps = budget;
                if let Some(l) = &link {
                    l.set_budget(budget * 1_000_000);
                }
            }
            let tip = match avg {
                a if a < 5.0 => "共享建议 720P 30 帧「标准」",
                a if a < 10.0 => "共享建议 1080P 60 帧「标准」",
                a if a < 20.0 => "共享建议 1080P 60 帧「高」",
                _ => "共享用 1080P 60 帧「极高」也够",
            };
            ui.label(RichText::new(format!("上行约 {avg:.1} Mbps（去掉离谱的低值后平均）：已把「直连上行预算」设成 {budget} Mbps；{tip}")).color(th::GREEN()));
        } else if !running && !rows.is_empty() {
            ui.label(RichText::new("都没测出来，上行预算没改").color(th::YELLOW()));
        }
        ui.add_space(8.0);
    }

    // ============================================================ 界面大小 / 迷你模式（第 84 项）

    /// 按窗口大小缩放界面：默认窗口（1180 × 760）= 100%，按宽、高里比例小的那个算，25%～160%（在 Windows 显示缩放之上再乘）；
    /// 变化不到 5% 不改（拖窗口边时字不会一直抖）。要缩到 50% 以下就换迷你模式（字保持正常大小）。设置里选了固定大小就不跟窗口变
    fn auto_zoom(&mut self, ctx: &egui::Context) {
        let cur = ctx.zoom_factor();
        // 窗口本身有多大（按 100% 算的点数）
        let size = ctx.content_rect().size() * cur;
        if size.x < 10.0 || size.y < 10.0 {
            return;
        }
        let raw = (size.x / BASE_W).min(size.y / BASE_H);
        let mini = raw < MINI_BELOW && matches!(self.screen, Screen::Main(_));
        let want = if mini {
            // 迷你模式按它自己的大小（约 300 × 240）缩，最多 100%
            (size.x / 300.0).min(size.y / 240.0).clamp(0.6, 1.0)
        } else if self.cfg.ui_scale == 0 {
            raw.clamp(0.25, 1.6)
        } else {
            self.cfg.ui_scale.clamp(50, 200) as f32 / 100.0
        };
        if mini != self.mini {
            self.mini = mini;
            ctx.set_zoom_factor(want);
            return;
        }
        if ((want - cur) / cur).abs() > 0.05 || (self.cfg.ui_scale != 0 && (want - cur).abs() > 0.001) {
            ctx.set_zoom_factor(want);
        }
    }

    /// 迷你模式：频道名、这个频道里的人（说话时亮起、闭麦 / 静音的标记）、闭麦 / 静音 / 挂断
    fn ui_mini(&mut self, ui: &mut egui::Ui) {
        let Screen::Main(s) = &mut self.screen else { return };
        let me = s.me().cloned();
        let my_ch = me.as_ref().and_then(|u| u.channel_id);
        let mut toggled = false;
        egui::Frame::new().fill(th::BG_PANEL()).inner_margin(Margin::same(8)).show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            let Some(ch) = my_ch else {
                ui.label(RichText::new("没在语音频道").strong());
                ui.label(RichText::new(&s.server_name).size(12.0).color(th::TEXT_DIM()));
                ui.label(RichText::new("把窗口拉大可以看到完整界面").size(11.0).color(th::TEXT_FAINT()));
                return;
            };
            let ch_name = s.channels.iter().find(|c| c.id == ch).map(|c| c.name.clone()).unwrap_or_default();
            ui.horizontal(|ui| {
                crate::icons::show(ui, Icon::Speaker, 14.0, th::TEXT_FAINT());
                ui.add(egui::Label::new(RichText::new(ch_name).strong()).truncate());
            });
            ui.separator();
            let members: Vec<UserInfo> = s.users.iter().filter(|u| u.channel_id == Some(ch)).cloned().collect();
            let bar_h = 44.0;
            egui::ScrollArea::vertical().max_height((ui.available_height() - bar_h).max(20.0)).auto_shrink([false, true]).show(ui, |ui| {
                for u in &members {
                    let is_me = Some(u.id) == me.as_ref().map(|m| m.id);
                    let talking = if is_me { self.settings.transmitting.load(Ordering::Relaxed) } else { self.voice.is_speaking(u.id) };
                    ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                        ui.painter().circle_filled(r.center(), 4.5, if talking { th::GREEN() } else { th::BG_ACTIVE() });
                        let name = if is_me { format!("{}（我）", u.name) } else { u.name.clone() };
                        ui.add(egui::Label::new(RichText::new(name).color(if talking { th::GREEN() } else { th::TEXT() })).truncate());
                        if u.deafened {
                            crate::icons::show(ui, Icon::HeadphonesOff, 13.0, th::RED());
                        } else if u.muted {
                            crate::icons::show(ui, Icon::MicOff, 13.0, th::RED());
                        }
                    });
                }
            });
            ui.with_layout(Layout::bottom_up(Align::Center), |ui| {
                ui.horizontal(|ui| {
                    let muted = self.settings.muted.load(Ordering::Relaxed);
                    let deaf = self.settings.deafened.load(Ordering::Relaxed);
                    if th::icon_button(ui, if muted { Icon::MicOff } else { Icon::Mic }, if muted { "取消闭麦" } else { "闭麦" }, if muted { Tone::Danger } else { Tone::Normal }).clicked() {
                        self.settings.muted.store(!muted, Ordering::Relaxed);
                        toggled = true;
                    }
                    if th::icon_button(ui, if deaf { Icon::HeadphonesOff } else { Icon::Headphones }, if deaf { "取消静音" } else { "静音：听不到别人" }, if deaf { Tone::Danger } else { Tone::Normal }).clicked() {
                        self.settings.deafened.store(!deaf, Ordering::Relaxed);
                        toggled = true;
                    }
                    if th::icon_button(ui, Icon::Leave, "挂断（离开语音频道）", Tone::Danger).clicked() {
                        s.send(ClientMsg::LeaveChannel);
                    }
                });
            });
        });
        // 说话的人亮起来要及时刷新
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        if toggled {
            self.sync_voice_state();
        }
    }

    // ============================================================ 外观页（第 59 项）

    fn ui_appearance_page(&mut self, ui: &mut egui::Ui) {
        th::section(ui, "主题");
        th::segmented(ui, &mut self.cfg.theme, &[(0u8, "跟随系统"), (1, "浅色"), (2, "深色")]);
        ui.label(RichText::new("「跟随系统」按 Windows 设置 → 个性化 → 颜色里的「应用模式」。").size(11.5).color(th::TEXT_FAINT()));
        ui.add_space(10.0);
        th::section(ui, "配色");
        let opts: Vec<(u8, &str)> = th::STYLES.iter().enumerate().map(|(i, s)| (i as u8, s.0)).collect();
        th::segmented(ui, &mut self.cfg.palette, &opts);
        ui.add_space(10.0);
        // 界面大小（第 84 项）
        th::section(ui, "界面大小");
        th::segmented(ui, &mut self.cfg.ui_scale, &[(0u32, "自动"), (90, "90%"), (100, "100%"), (110, "110%"), (125, "125%"), (150, "150%")]);
        ui.label(
            RichText::new("「自动」：窗口拉大字和按钮跟着变大，拉小跟着变小（25%～160%）；小到一定程度换成迷你模式，只留频道里的人和闭麦 / 静音 / 挂断。")
                .size(11.5)
                .color(th::TEXT_FAINT()),
        );
        ui.add_space(10.0);
        th::section(ui, "毛玻璃效果");
        let r = ui.checkbox(&mut self.cfg.glass, "毛玻璃效果").on_hover_text(
            "打开后：弹出设置、共享这类窗口时，后面的界面变成模糊的磨砂玻璃。
             只在弹框打开那一刻算一次，打游戏时也可以开。",
        );
        if r.changed() {
            self.glass_backdrop = None;
            self.glass_pending = None;
            crate::glass::remember(self.cfg.glass);
        }
        // 窗口背景的云母效果先停用（第 76 项：拖动 / 放大窗口后有一块灰色方块）
        let note = "弹框后面的模糊马上生效。（以前 Windows 11 上窗口背景的云母效果先停用了：拖动、放大窗口后会出一块灰色方块。）";
        ui.label(RichText::new(note).size(11.5).color(th::TEXT_FAINT()));
    }

    // ============================================================ 按键页（第 58 项）

    /// 设置 → 按键：说话方式、一张动作表（按键 · 范围 · 更改 / 清除 / 默认）、拖尾、冲突提示
    fn ui_keys_page(&mut self, ui: &mut egui::Ui) {
        use crate::hotkey::{self as hk, Hotkey};
        th::section(ui, "说话方式");
        let mut ptt = self.cfg.ptt_on;
        if th::segmented(ui, &mut ptt, &[(false, "语音激活"), (true, "按键说话")]) {
            if ptt && !self.cfg.ptt_key.is_set() {
                // 还没设说话键：先录，录好了才切过去（取消就留在语音激活）
                self.begin_key_edit(hk::PTT);
            } else {
                self.cfg.ptt_on = ptt;
                self.apply_hotkeys();
            }
        }
        ui.label(
            RichText::new(if self.cfg.ptt_on {
                "按住说话键才发声，松开停；说话键可以带 Ctrl / Shift / Alt，也可以是鼠标中键、侧键。"
            } else {
                "声音超过灵敏度阈值就发（阈值在「音频」页调）。"
            })
            .size(11.5)
            .color(th::TEXT_FAINT()),
        );

        ui.add_space(8.0);
        th::section(ui, "按键");
        let keys = self.cfg.keys();
        let mut clear: Option<usize> = None;
        let mut reset: Option<usize> = None;
        let mut edit: Option<usize> = None;
        let mut scope: Option<(usize, bool)> = None;
        egui::Grid::new("keys").num_columns(4).spacing([10.0, 8.0]).striped(true).show(ui, |ui| {
            for (i, (name, tip)) in hk::ACTIONS.iter().enumerate() {
                let k = keys[i];
                let dim = i == hk::PTT && !self.cfg.ptt_on;
                let mut n = RichText::new(*name);
                if dim {
                    n = n.color(th::TEXT_FAINT());
                }
                ui.label(n).on_hover_text(if dim { "现在是语音激活：这个键不起作用，切到「按键说话」才用" } else { tip });
                let mut l = RichText::new(k.label()).monospace();
                l = if k.is_set() { l.strong() } else { l.color(th::TEXT_FAINT()) };
                ui.label(l);
                let mut fo = k.focus_only;
                ui.add_enabled_ui(k.is_set(), |ui| {
                    egui::ComboBox::from_id_salt(("scope", i)).width(92.0).selected_text(if fo { "仅 VOC 前台" } else { "全局" }).show_ui(ui, |ui| {
                        ui.selectable_value(&mut fo, false, "全局").on_hover_text("游戏、浏览器在前台时也有效");
                        ui.selectable_value(&mut fo, true, "仅 VOC 前台").on_hover_text("只有 VOC 的窗口（含弹出的观看窗口）在最前面时才有效");
                    });
                });
                if fo != k.focus_only {
                    scope = Some((i, fo));
                }
                ui.horizontal(|ui| {
                    if ui.small_button("更改").clicked() {
                        edit = Some(i);
                    }
                    if k.is_set() && ui.small_button("清除").clicked() {
                        clear = Some(i);
                    }
                    let d = hk::default_key(i);
                    if d.is_set() && !d.same_combo(&k) && ui.small_button("默认").on_hover_text(format!("改回 {}", d.label())).clicked() {
                        reset = Some(i);
                    }
                });
                ui.end_row();
            }
        });
        if let Some(i) = edit {
            self.begin_key_edit(i);
        }
        if let Some(i) = clear {
            *self.cfg.key_mut(i) = Hotkey::default();
            if i == hk::PTT {
                // 说话键清掉了就回到语音激活，免得永远发不出声
                self.cfg.ptt_on = false;
            }
            self.apply_hotkeys();
        }
        if let Some(i) = reset {
            let d = hk::default_key(i);
            let keys = self.cfg.keys();
            match hk::check(i, &d, &keys, false) {
                Some(hk::Problem::Dup(j)) | Some(hk::Problem::Overlap(j)) => {
                    self.key_note = Some(format!("{} 已经用于「{}」，先改掉那个再恢复默认", d.label(), hk::ACTIONS[j].0));
                }
                _ => {
                    let fo = self.cfg.key_mut(i).focus_only;
                    *self.cfg.key_mut(i) = Hotkey { focus_only: fo, ..d };
                    self.apply_hotkeys();
                }
            }
        }
        if let Some((i, fo)) = scope {
            self.cfg.key_mut(i).focus_only = fo;
            self.apply_hotkeys();
        }

        if self.cfg.ptt_on {
            ui.add_space(6.0);
            let mut tail = self.cfg.ptt_tail_ms as f32;
            if ui
                .add(egui::Slider::new(&mut tail, 0.0..=1000.0).step_by(50.0).suffix(" ms").text("松开后多发"))
                .on_hover_text("松开说话键后再多发一会儿，句尾不会被截断。0 = 松开立刻停")
                .changed()
            {
                self.cfg.ptt_tail_ms = tail as u32;
                self.apply_hotkeys();
            }
        }

        // 冲突、容易误触的提示（老配置留下的也在这里显示）
        let keys = self.cfg.keys();
        for (i, p) in hk::current_problems(&keys) {
            if i == hk::PTT && !self.cfg.ptt_on {
                continue;
            }
            let msg = match p {
                hk::Problem::Dup(j) => format!("「{}」和「{}」用了同一个键 {}，请改掉一个", hk::ACTIONS[i].0, hk::ACTIONS[j].0, keys[i].label()),
                hk::Problem::Overlap(j) => {
                    let (p, t) = if i == hk::PTT { (i, j) } else { (j, i) };
                    format!("按「{}」（{}）时也会触发按键说话（{}）", hk::ACTIONS[t].0, keys[t].label(), keys[p].label())
                }
                hk::Problem::System(name) => format!("「{}」用的 {} 是系统的「{name}」，建议换一个", hk::ACTIONS[i].0, keys[i].label()),
                hk::Problem::Typing => format!("「{}」用的 {} 打字时也会触发，建议加上 Ctrl 或 Alt", hk::ACTIONS[i].0, keys[i].label()),
                hk::Problem::Win => continue,
            };
            ui.label(RichText::new(msg).size(11.5).color(th::YELLOW()));
        }
        for i in 1..keys.len() {
            if hk::risky_global(&keys[i]) {
                ui.label(
                    RichText::new(format!(
                        "「{}」用的 {} 在浏览器、编辑器等很多软件里也有用途，在那些软件里按也会触发；建议加上 Shift，或改成「仅 VOC 前台」",
                        hk::ACTIONS[i].0,
                        keys[i].label()
                    ))
                    .size(11.5)
                    .color(th::TEXT_FAINT()),
                );
            }
        }
        if let Some(n) = &self.key_note {
            ui.label(RichText::new(n).size(11.5).color(th::YELLOW()));
        }
        ui.label(
            RichText::new("「全局」的键在游戏里也有效：VOC 只是查按键状态，不会吃掉按键，游戏照样收得到。切换类快捷键按下时响一声（低音 = 闭麦 / 静音 / 停止共享，高音 = 恢复 / 开始共享）。")
                .size(11.5)
                .color(th::TEXT_FAINT()),
        );
        ui.add_space(4.0);
        if ui.button("快捷键全部恢复默认").on_hover_text("闭麦、静音、共享、弹出窗口的键和范围恢复默认，拖尾恢复 200 ms；按键说话的键不动").clicked() {
            let ptt = self.cfg.ptt_key;
            for i in 1..hk::ACTIONS.len() {
                *self.cfg.key_mut(i) = hk::default_key(i);
            }
            // 默认的闭麦 / 静音键和说话键撞了：说话键优先，撞的那个清掉
            for i in 1..hk::ACTIONS.len() {
                let keys = self.cfg.keys();
                if self.cfg.ptt_on && matches!(hk::check(i, &keys[i], &keys, false), Some(hk::Problem::Dup(hk::PTT) | hk::Problem::Overlap(hk::PTT))) {
                    *self.cfg.key_mut(i) = Hotkey::default();
                }
            }
            self.cfg.ptt_key = ptt;
            self.cfg.ptt_tail_ms = 200;
            self.key_note = None;
            self.apply_hotkeys();
        }
    }

    /// 打开录键框：所有全局按键（含按键说话）暂停
    pub(crate) fn begin_key_edit(&mut self, action: usize) {
        self.key_edit = Some(crate::app::KeyEdit { action, cap: crate::hotkey::Capture::new(action == crate::hotkey::PTT), got: None });
        self.key_note = None;
        self.hotkeys.paused.store(true, Ordering::Relaxed);
        self.settings.keys_paused.store(true, Ordering::Relaxed);
    }

    /// 关掉录键框。`set` = 要设的键（`swap` 时把原来的键换给撞上的那个动作）
    fn end_key_edit(&mut self, set: Option<(crate::hotkey::Hotkey, Option<usize>)>) {
        use crate::hotkey as hk;
        let Some(ed) = self.key_edit.take() else { return };
        if let Some((k, swap)) = set {
            let old = *self.cfg.key_mut(ed.action);
            if let Some(j) = swap {
                let fo = self.cfg.key_mut(j).focus_only;
                *self.cfg.key_mut(j) = hk::Hotkey { focus_only: fo, ..old };
                if j == hk::PTT && !old.is_set() {
                    self.cfg.ptt_on = false;
                }
            }
            *self.cfg.key_mut(ed.action) = hk::Hotkey { focus_only: old.focus_only, ..k };
            if ed.action == hk::PTT {
                self.cfg.ptt_on = true;
            }
        }
        self.hotkeys.paused.store(false, Ordering::Relaxed);
        self.settings.keys_paused.store(false, Ordering::Relaxed);
        self.apply_hotkeys();
    }

    /// 录键框：实时显示正按着的组合，全部松开才确定；冲突时问「换过来 / 仍然使用 / 重新录」
    fn ui_key_edit(&mut self, ctx: &egui::Context) {
        use crate::hotkey::{self as hk, Problem, Step};
        let Some(ed) = self.key_edit.as_mut() else { return };
        let action = ed.action;
        let mut live = String::new();
        let mut done: Option<hk::Hotkey> = None;
        let mut cancel = false;
        if ed.got.is_none() {
            ctx.request_repaint();
            match ed.cap.poll() {
                Step::Waiting => {}
                Step::Live(t) => live = t,
                Step::Done(k) => done = Some(k),
                Step::Cancel => cancel = true,
            }
        }
        if let Some(k) = done {
            let keys = self.cfg.keys();
            match hk::check(action, &k, &keys, ed.cap.win) {
                None => {
                    self.end_key_edit(Some((k, None)));
                    return;
                }
                Some(p) => ed.got = Some((k, p)),
            }
        }
        let ed = self.key_edit.as_ref().unwrap();
        let got = ed.got;
        let mut act: Option<u8> = None; // 0 取消，1 重新录，2 换过来，3 仍然使用
        egui::Window::new(RichText::new(format!("设置按键：{}", hk::ACTIONS[action].0)).size(15.0).strong())
            .id(egui::Id::unique("key-edit"))
            .collapsible(false)
            .resizable(false)
            .order(egui::Order::Foreground)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_width(340.0);
                match got {
                    None => {
                        ui.label(if action == hk::PTT {
                            "按下要用来说话的键：键盘键、鼠标中键或侧键，可以带 Ctrl / Shift / Alt，也可以只用一个 Ctrl / Shift / Alt。"
                        } else {
                            "按下快捷键组合（建议带 Ctrl / Alt，例如 Ctrl+Shift+M、F8、鼠标侧键），全部松开后确定。"
                        });
                        ui.add_space(10.0);
                        let shown = if live.is_empty() { "等你按键…".to_string() } else { live.clone() };
                        ui.vertical_centered(|ui| {
                            ui.label(RichText::new(shown).size(22.0).strong().color(if live.is_empty() { th::TEXT_FAINT() } else { th::ACCENT_HOVER() }));
                        });
                        ui.add_space(10.0);
                        ui.label(RichText::new("录键时所有快捷键和按键说话都暂停。Esc 或「取消」不改。").size(11.5).color(th::TEXT_FAINT()));
                        if ui.button("取消").clicked() {
                            act = Some(0);
                        }
                    }
                    Some((k, p)) => {
                        let msg = match p {
                            Problem::Win => "带 Win 键的组合是 Windows 的系统快捷键，不能用。".to_string(),
                            Problem::System(name) => format!("{} 是常用的「{name}」快捷键，不能用。", k.label()),
                            Problem::Typing => format!("{} 打字时也会触发（聊天、在别的软件里打字都会），请加上 Ctrl 或 Alt。", k.label()),
                            Problem::Dup(j) => format!("{} 已经用于「{}」。要换过来吗？（「{}」会改用原来的 {}）", k.label(), hk::ACTIONS[j].0, hk::ACTIONS[j].0, self.cfg.keys()[action].label()),
                            Problem::Overlap(j) => {
                                let other = self.cfg.keys()[j];
                                if action == hk::PTT {
                                    format!("按「{}」（{}）时也会让你说话。仍然使用 {} 吗？", hk::ACTIONS[j].0, other.label(), k.label())
                                } else {
                                    format!("按 {} 时也会触发按键说话（{}）。仍然使用吗？", k.label(), other.label())
                                }
                            }
                        };
                        ui.label(RichText::new(msg).color(if p.blocking() { th::RED() } else { th::YELLOW() }));
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if let Problem::Dup(_) = p {
                                if ui.button("换过来").clicked() {
                                    act = Some(2);
                                }
                            }
                            if let Problem::Overlap(_) = p {
                                if ui.button("仍然使用").clicked() {
                                    act = Some(3);
                                }
                            }
                            if ui.button("重新录").clicked() {
                                act = Some(1);
                            }
                            if ui.button("取消").clicked() {
                                act = Some(0);
                            }
                        });
                    }
                }
            });
        if cancel {
            act = Some(0);
        }
        match act {
            Some(0) => self.end_key_edit(None),
            Some(1) => {
                let ed = self.key_edit.as_mut().unwrap();
                ed.cap = hk::Capture::new(action == hk::PTT);
                ed.got = None;
            }
            Some(2) => {
                if let Some((k, Problem::Dup(j))) = got {
                    self.end_key_edit(Some((k, Some(j))));
                }
            }
            Some(3) => {
                if let Some((k, _)) = got {
                    self.end_key_edit(Some((k, None)));
                }
            }
            _ => {}
        }
    }

    // ============================================================ 屏幕共享

    fn ui_share(&mut self, ctx: &egui::Context) {
        if !self.show_share {
            return;
        }
        let Screen::Main(s) = &mut self.screen else { return };
        if self.monitors.is_empty() {
            self.monitors = video::monitor_names();
        }
        let mut open = true;
        let mut start = false;
        egui::Window::new(RichText::new("屏幕共享").size(17.0).strong())
            .id(egui::Id::unique("share"))
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .pivot(Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .constrain(true)
            .show(ctx, |ui| {
                ui.set_width(400.0);
                // 内容放进滚动区（第 74 项：VOC 窗口不够高时以前上下被截掉，标题和「开始共享」都看不到）；「开始共享」固定在下面
                let max_h = (ctx.content_rect().height() - 150.0).max(160.0);
                egui::ScrollArea::vertical().max_height(max_h).auto_shrink([false, true]).show(ui, |ui| {
                    // 系统不支持的截屏设置（第 79 项）：照样能共享，告诉共享者有什么不一样
                    let limits = video::capture_limits();
                    if !limits.is_empty() {
                        let text = format!("你的 Windows 版本较旧，有几项截屏设置用不了（照样能共享）：
· {}", limits.join("
· "));
                        ui.label(RichText::new(text).size(11.5).color(th::YELLOW()));
                        ui.add_space(6.0);
                    }
                    if let Some(sh) = &self.sharer {
                        let st = &sh.stats;
                        th::pill(ui, format!("● 正在共享 {}", sh.cfg.desc()), th::GREEN(), th::GREEN_SOFT());
                        ui.add_space(6.0);
                        let watchers = self.voice.current_link().map(|l| l.watcher_count()).unwrap_or(0);
                        if watchers == 0 {
                            ui.label(RichText::new("还没有人在看（没人看时不编码，不占流量）").color(th::TEXT_DIM()));
                        } else {
                            egui::Grid::new("sharestats").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
                                ui.label(RichText::new("观看").color(th::TEXT_DIM()));
                                ui.label(format!("{watchers} 人"));
                                ui.end_row();
                                ui.label(RichText::new("分辨率").color(th::TEXT_DIM()));
                                ui.label(format!("{}×{}", st.width.load(Ordering::Relaxed), st.height.load(Ordering::Relaxed)));
                                ui.end_row();
                                ui.label(RichText::new("帧率").color(th::TEXT_DIM()));
                                ui.label(format!("{} fps（采集 {}）", st.fps.load(Ordering::Relaxed), st.capture_fps.load(Ordering::Relaxed)))
                                    .on_hover_text(format!(
                                        "系统这一秒给了 {} 帧画面。系统只在屏幕内容变化时给帧：比所选帧率低，说明屏幕 / 游戏本身就没那么多帧，不是 VOC 丢的",
                                        st.source_fps.load(Ordering::Relaxed)
                                    ));
                                ui.end_row();
                                ui.label(RichText::new("码率").color(th::TEXT_DIM()));
                                let mbps = st.kbps.load(Ordering::Relaxed) as f32 / 1000.0;
                                let skipped = st.static_skipped.load(Ordering::Relaxed);
                                let auto_h = st.auto_height.load(Ordering::Relaxed);
                                let fec = match st.fec_pct.load(Ordering::Relaxed) {
                                    0 => String::new(),
                                    p => format!(" · 有丢包，已加 {p}% 冗余"),
                                };
                                let pct = st.rate_pct.load(Ordering::Relaxed);
                                if auto_h > 0 {
                                    ui.label(RichText::new(format!("{mbps:.1} Mbps · 网络拥堵，已自动降到 {auto_h}P{fec}")).color(th::YELLOW()));
                                } else if pct < 100 && st.low_watchers.load(Ordering::Relaxed) > 0 {
                                    ui.label(RichText::new(format!("{mbps:.1} Mbps · 流畅层已自动降到 {pct}%{fec}")).color(th::YELLOW()));
                                } else if fec.is_empty() {
                                    ui.label(format!("{mbps:.1} Mbps"));
                                } else {
                                    ui.label(RichText::new(format!("{mbps:.1} Mbps{fec}")).color(th::YELLOW()));
                                }
                                ui.end_row();
                                if skipped > 0 {
                                    ui.label(RichText::new("省流量").color(th::TEXT_DIM()));
                                    ui.label(format!("画面变化小，每秒少发 {skipped} 帧"));
                                    ui.end_row();
                                }
                                ui.label(RichText::new("编码").color(th::TEXT_DIM()));
                                ui.label(format!("{} · {} ms/帧", st.encoder.lock().unwrap(), st.encode_ms.load(Ordering::Relaxed)));
                                ui.end_row();
                                ui.label(RichText::new("画面处理").color(th::TEXT_DIM()));
                                let pipe = st.pipeline.lock().unwrap().clone();
                                let tip = if pipe == "显卡直通" {
                                    "画面全程在显卡上缩放、转换、编码，不拷回内存，打游戏时也不抢 CPU"
                                } else {
                                    "画面拷回内存用 CPU 转换（编码器和显示器不在同一块显卡上、或用软件编码时）"
                                };
                                ui.label(pipe).on_hover_text(tip);
                                ui.end_row();
                            });
                        }
                        if let Some(w) = &self.share_bw_warn {
                            egui::Frame::new().fill(th::YELLOW_SOFT()).corner_radius(CornerRadius::same(8)).inner_margin(Margin::symmetric(10, 6)).show(ui, |ui| {
                                ui.label(RichText::new(format!("⚠ {w}")).color(th::YELLOW()));
                            });
                        }
                        if sh.cfg.force_av1 {
                            let names = cannot_av1(s);
                            if !names.is_empty() {
                                ui.label(RichText::new(format!("强制 AV1：{} 解不了，看不了你的共享", names.join("、"))).size(11.5).color(th::YELLOW()));
                            }
                        }
                        if let Some(e) = st.error.lock().unwrap().as_ref() {
                            ui.label(RichText::new(e).color(th::RED()));
                        }
                        if self.cfg.share_audio && crate::loopback::supported() {
                            ui.add_space(4.0);
                            let p2p_only = sh.cfg.p2p_only;
                            share_audio_route_ui(ui, &mut self.cfg.share_audio_route, p2p_only);
                            ui.checkbox(&mut self.cfg.stream_stereo, "共享声音用立体声").on_hover_text("左右声道分开发；码率 128 kbps，单声道 96 kbps");
                        }
                        ui.add_space(8.0);
                        let b = egui::Button::new(RichText::new("⏹ 停止共享").strong().color(Color32::WHITE))
                            .fill(th::RED())
                            .corner_radius(CornerRadius::same(10))
                            .min_size(Vec2::new(ui.available_width(), 36.0));
                        if ui.add(b).clicked() {
                            self.sharer = None;
                            self.share_confirmed = false;
                            s.send(ClientMsg::ShareStop);
                        }
                        return;
                    }
                    // 共享什么（第 60-1 项）：整个显示器，或者只共享一个窗口（切出去的微信、浏览器不会被看到）
                    th::section(ui, "共享什么");
                    let was = self.share_pick_window;
                    th::segmented(ui, &mut self.share_pick_window, &[(false, "整个显示器"), (true, "一个窗口")]);
                    if self.share_pick_window && (!was || self.share_windows.is_empty()) {
                        self.share_windows = video::window_list();
                    }
                    if self.share_pick_window {
                        ui.horizontal(|ui| {
                            let cur = self.cfg.share.window.as_ref().map(|w| format!("{} — {}", w.title, w.exe)).unwrap_or_else(|| "选一个窗口…".into());
                            egui::ComboBox::from_id_salt("win").width(ui.available_width() - 60.0).selected_text(cur).show_ui(ui, |ui| {
                                for w in &self.share_windows {
                                    let label = format!("{} — {}", w.title, w.exe);
                                    if ui.selectable_label(self.cfg.share.window.as_ref().is_some_and(|c| c.hwnd == w.hwnd), label).clicked() {
                                        self.cfg.share.window = Some(w.clone());
                                    }
                                }
                            });
                            if ui.button("刷新").on_hover_text("重新列出窗口").clicked() {
                                self.share_windows = video::window_list();
                            }
                        });
                        ui.label(RichText::new("只共享这个窗口：被别的窗口挡住也照样看得到它；最小化时观看者那边会提示。").size(11.5).color(th::TEXT_FAINT()));
                    } else {
                        self.cfg.share.window = None;
                        let cur = self.monitors.get(self.cfg.share.monitor).cloned().unwrap_or_else(|| "主显示器".into());
                        egui::ComboBox::from_id_salt("mon").width(ui.available_width()).selected_text(cur).show_ui(ui, |ui| {
                            for (i, m) in self.monitors.iter().enumerate() {
                                ui.selectable_value(&mut self.cfg.share.monitor, i, m);
                            }
                        });
                    }
                    ui.add_space(6.0);
                    th::section(ui, "分辨率");
                    th::segmented(ui, &mut self.cfg.share.height, &[(480, "480P"), (720, "720P"), (1080, "1080P")]);
                    ui.add_space(6.0);
                    th::section(ui, "帧率");
                    // 120 帧（试验，第 64 项）：只能 AV1；要有 AV1 硬件编码器、共享的屏幕刷新率要到 120 Hz
                    let refresh = match &self.cfg.share.window {
                        Some(w) => video::window_refresh(w.hwnd),
                        None => video::monitor_refresh(self.cfg.share.monitor),
                    };
                    let av1 = video::can_encode_av1();
                    let can120 = av1 && refresh >= 119;
                    let opts: Vec<(u32, &str)> = if can120 { vec![(30, "30 帧"), (60, "60 帧"), (120, "120 帧（试验）")] } else { vec![(30, "30 帧"), (60, "60 帧")] };
                    th::segmented(ui, &mut self.cfg.share.fps, &opts);
                    if !can120 {
                        if self.cfg.share.fps > 60 {
                            self.cfg.share.fps = 60;
                        }
                        let why = if !av1 { "显卡没有 AV1 硬件编码".to_string() } else { format!("共享的屏幕刷新率只有 {refresh} Hz（要 120 Hz 以上）") };
                        ui.label(RichText::new(format!("120 帧（试验）用不了：{why}")).size(11.5).color(th::TEXT_FAINT()));
                    } else if self.cfg.share.fps > 60 {
                        ui.label(
                            RichText::new("试验功能：只用 AV1（解不了 AV1 的人看不了），码率约为 60 帧的 1.6 倍；上行不够时会提醒你。观看的人屏幕也要 120 Hz 才看得出差别。")
                                .size(11.5)
                                .color(th::YELLOW()),
                        );
                    }
                    ui.add_space(6.0);
                    th::section(ui, "画质");
                    let q: Vec<(u8, &str)> = video::QUALITIES.iter().enumerate().map(|(i, q)| (i as u8, q.0)).collect();
                    th::segmented(ui, &mut self.cfg.share.quality, &q);
                    ui.add_space(6.0);
                    ui.checkbox(&mut self.cfg.share.skip_static, "画面变化小时少发（省流量）")
                        .on_hover_text("画面只有一小块在变（时钟、打字、鼠标移动）时最多每秒发 10 帧，完全不动时每秒 1 帧；大范围变化（游戏、视频）照常满帧。\n看网页、文档、游戏菜单时能省不少带宽，代价是这时鼠标移动看起来没那么顺。\n需要较新的 Windows 11（能报告屏幕哪里变了），不支持时这个开关不起作用。");
                    ui.checkbox(&mut self.cfg.share.auto_res, "网络差时自动降低分辨率")
                        .on_hover_text("网络拥堵时自动把分辨率从 1080P 降到 900P、720P、540P，码率跟着降，比硬压码率清楚；网络好了自动升回来。\n打开后选「原画」的人也会跟着变。");
                    ui.add_space(6.0);
                    th::section(ui, "编码器");
                    let choices = video::encoder_choices();
                    let cur = choices.iter().find(|c| c.0 == self.cfg.share.encoder).map(|c| c.1.clone()).unwrap_or_else(|| choices[0].1.clone());
                    egui::ComboBox::from_id_salt("share-encoder").width(ui.available_width()).selected_text(cur).show_ui(ui, |ui| {
                        for (k, label) in &choices {
                            ui.selectable_value(&mut self.cfg.share.encoder, k.clone(), label);
                        }
                    });
                    ui.add_space(8.0);
                    let eff = self.cfg.share.effective(self.cfg.low_bandwidth);
                    if eff != self.cfg.share {
                        ui.label(RichText::new(format!("低带宽模式：实际按 {} 共享", eff.desc())).size(12.0).color(th::YELLOW()));
                    }
                    ui.label(
                        RichText::new(format!(
                            "每位观看者约 {:.1} Mbps（H.264；AV1 约 {:.1}）· 端到端加密 · 能直连的不经过服务器",
                            eff.bitrate() as f32 / 1e6,
                            eff.bitrate_for(crate::mf::enc::Codec::Av1) as f32 / 1e6
                        ))
                        .size(12.0)
                        .color(th::TEXT_DIM()),
                    );
                    if let Some(e) = &self.share_error {
                        ui.label(RichText::new(e).color(th::RED()));
                    }
                    ui.add_space(6.0);
                    let can_audio = crate::loopback::supported();
                    ui.add_enabled(can_audio, egui::Checkbox::new(&mut self.cfg.share_audio, "同时共享电脑声音（不含 VOC 自己的声音）"))
                        .on_hover_text("游戏、视频的声音会一起发给正在看的人；队友的说话声、提示音不会被录进去。")
                        .on_disabled_hover_text("共享电脑声音需要 Windows 11");
                    if self.cfg.share_audio && can_audio {
                        if self.share_pick_window {
                            ui.checkbox(&mut self.cfg.share.audio_app_only, "共享声音只录这个窗口的程序")
                                .on_hover_text("只录被共享的那个程序的声音（你自己放的音乐、其他软件的提示音不会被录进去）。取消勾选 = 录除了 VOC 以外的全部声音");
                        }
                        share_audio_route_ui(ui, &mut self.cfg.share_audio_route, self.cfg.share.p2p_only);
                        ui.checkbox(&mut self.cfg.stream_stereo, "共享声音用立体声")
                            .on_hover_text("左右声道分开发（游戏里听方位更准）；码率 128 kbps，单声道 96 kbps。伴奏也一样。\n旧版本的观看者收到会自动混成单声道");
                    }
                    ui.checkbox(&mut self.cfg.share.delay_bwe, "按延迟变化提前降码率").on_hover_text(
                        "画面走的线路往返时间开始变大（排队了、还没丢包）就先降码率，线路空了再升回来，减少卡顿。\n\
                         打开后「原画」的码率也会跟着调（默认原画固定码率、保证清楚）。",
                    );
                    // 强制 AV1（第 61 项）：本机没有 AV1 硬件编码器就变灰
                    let av1 = video::can_encode_av1();
                    if self.cfg.share.fps > 60 {
                        self.cfg.share.force_av1 = true;
                    }
                    ui.add_enabled(av1 && self.cfg.share.fps <= 60, egui::Checkbox::new(&mut self.cfg.share.force_av1, "强制用 AV1 编码"))
                        .on_hover_text("不管观看者能不能解，都用 AV1（同样码率更清楚）。解不了 AV1 的人看不了，他那边会写原因")
                        .on_disabled_hover_text("这台电脑的显卡没有 AV1 硬件编码（要 NVIDIA RTX 40 系、AMD RX 7000、Intel Arc 及以后），用不了");
                    if !av1 {
                        self.cfg.share.force_av1 = false;
                    } else if self.cfg.share.force_av1 {
                        let names = cannot_av1(s);
                        if !names.is_empty() {
                            ui.label(RichText::new(format!("{} 解不了 AV1，开着这项他们看不了你的共享", names.join("、"))).size(11.5).color(th::YELLOW()));
                        }
                    }
                    ui.checkbox(&mut self.cfg.share.p2p_only, "只通过 P2P 直连发送（不经服务器）").on_hover_text(
                        "画面、补发、鼠标、共享声音都不发给服务器，服务器带宽小时有用。\n\
                         能打通直连的人全部直连发，不受「直连上行预算」限制：看的人多时可能占满你的上行。\n\
                         和你打不通、也没有别的观看者能转给他的人看不了（他那边会看到原因）。",
                    );
                });
                if self.sharer.is_none() {
                    ui.add_space(8.0);
                    start = th::primary_button(ui, "开始共享", ui.available_width(), true).clicked();
                }
            });
        if start && self.start_share().is_ok() {
            self.show_share = false;
        }
        if !open {
            self.show_share = false;
        }
    }

    /// 大图窗口（不提供保存：关闭 VOC 后图片就没了）
    fn ui_image_view(&mut self, ctx: &egui::Context) {
        let Some(tex) = self.image_view.clone() else { return };
        let mut open = true;
        let [w, h] = tex.size();
        egui::Window::new("图片")
            .open(&mut open)
            .resizable(true)
            .collapsible(false)
            .default_size(Vec2::new((w as f32).min(1000.0), (h as f32).min(700.0) + 30.0))
            .show(ctx, |ui| {
                ui.label(RichText::new(format!("{w} × {h} · 阅后即焚：关闭 VOC 后图片会被清除，无法保存")).size(11.5).color(th::TEXT_FAINT()));
                let avail = ui.available_size();
                ui.add(egui::Image::new(&tex).max_size(avail).maintain_aspect_ratio(true));
            });
        if !open || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.image_view = None;
        }
    }

    fn ui_viewers(&mut self, ctx: &egui::Context) {
        let Screen::Main(s) = &mut self.screen else { return };
        let mut close = Vec::new();
        // 观看中改了画质或线路：重新告诉服务器
        let mut relayer: Vec<(u32, bool, u8)> = Vec::new();
        // 改了播放方式：记住，下次看别人也用这个
        let mut smooth_change: Option<bool> = None;
        let mut buffer_change: Option<(u8, u32)> = None;
        // 共享声音跟着画面的缓冲（第 78 项）：不看了的人清掉额外缓冲
        {
            let alive: std::collections::HashSet<u32> = self.views.keys().copied().collect();
            for id in self.share_av_ids.difference(&alive) {
                self.mixer.lock().unwrap().set_extra_delay(crate::audio::stream_mixer_id(voc_proto::udp::SHARE_AUDIO, *id), 0.0);
            }
            for (id, va) in self.views.iter() {
                let v = va.lock().unwrap();
                sync_share_audio(&self.mixer, *id, &v.view);
            }
            self.share_av_ids = alive;
        }
        let voice = self.voice.clone();
        let mut go_full: Option<u32> = None;
        let mut exit_full = false;
        let mixer = self.mixer.clone();
        if ctx.input(|i| i.pointer.delta() != Vec2::ZERO) {
            self.fs_mouse_at = std::time::Instant::now();
        }
        for (id, va) in self.views.iter() {
            let mut v = va.lock().unwrap();
            {
                let user = s.users.iter().find(|u| u.id == *id);
                v.info = crate::app::ViewInfo {
                    name: user.map(|u| u.name.clone()).unwrap_or_default(),
                    desc: user.and_then(|u| u.sharing.clone()).unwrap_or_default(),
                    simulcast: user.is_some_and(|u| u.simulcast),
                    share_audio: user.is_some_and(|u| u.share_audio),
                    p2p_only: user.is_some_and(|u| u.share_p2p_only),
                };
            }
            let (name, desc) = (v.info.name.clone(), v.info.desc.clone());
            // 全屏：整个窗口只显示画面，鼠标 2 秒不动就隐藏工具栏
            if self.fullscreen == Some(*id) {
                *v.view.repaint_target.lock().unwrap() = None;
                refresh_viewer(ctx, *id, &mut v);
                let screen = ctx.content_rect();
                let show_bar = self.fs_mouse_at.elapsed() < std::time::Duration::from_secs(2);
                egui::Area::new(egui::Id::unique(("fullscreen", *id))).fixed_pos(screen.min).order(egui::Order::Foreground).show(ctx, |ui| {
                    ui.painter().rect_filled(screen, 0.0, Color32::BLACK);
                    let (rect, resp) = ui.allocate_exact_size(screen.size(), Sense::click());
                    if v.gpu.is_some() || v.tex.is_some() {
                        let (w, h) = (v.size.0.max(1) as f32, v.size.1.max(1) as f32);
                        let scale = (rect.width() / w).min(rect.height() / h);
                        let img = egui::Rect::from_center_size(rect.center(), Vec2::new(w * scale, h * scale));
                        match (&v.gpu, &v.tex) {
                            (Some(g), _) => crate::nv12view::paint(ui.painter(), img, *id, g.clone()),
                            (None, Some(t)) => {
                                ui.painter().image(t.id(), img, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
                            }
                            _ => {}
                        }
                        draw_remote_cursor(ui.painter(), img, &v.view);
                    }
                    if resp.double_clicked() {
                        exit_full = true;
                    }
                    if show_bar {
                        let bar = egui::Rect::from_min_size(rect.min, Vec2::new(rect.width(), 44.0));
                        ui.painter().rect_filled(bar, 0.0, Color32::from_black_alpha(170));
                        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(bar.shrink2(Vec2::new(16.0, 8.0))));
                        child.horizontal(|ui| {
                            ui.label(RichText::new(format!("📺 {name} 的屏幕 · {desc} · {} fps", v.shown_stats().0)).color(Color32::WHITE));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui.button("退出全屏").clicked() {
                                    exit_full = true;
                                }
                                if v.info.share_audio {
                                    let sid = crate::audio::stream_mixer_id(voc_proto::udp::SHARE_AUDIO, *id);
                                    let mut m = mixer.lock().unwrap();
                                    let mut vol = m.volume(sid);
                                    if th::vol_slider(ui, &mut vol, "🔊", 110.0).changed() {
                                        m.set_volume(sid, vol);
                                    }
                                }
                            });
                        });
                    }
                });
                if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                    exit_full = true;
                }
                continue;
            }
            if v.popped {
                // 独立窗口（第 53-4 项），自己刷新（第 56-b 项）：新帧到了解码线程直接唤醒这个窗口，不跟着主窗口一起排队画
                let vid = egui::ViewportId::from_hash_of(("viewer-window", *id));
                *v.view.repaint_target.lock().unwrap() = Some(vid);
                drop(v);
                let (va2, voice2, mixer2, id2) = (va.clone(), voice.clone(), mixer.clone(), *id);
                let title = format!("{name} 的屏幕 - VOC");
                ctx.show_viewport_deferred(
                    vid,
                    egui::ViewportBuilder::default().with_title(title).with_inner_size([1100.0, 680.0]).with_min_inner_size([480.0, 300.0]),
                    move |vctx, _class| {
                        let mut v = va2.lock().unwrap();
                        refresh_viewer(vctx, id2, &mut v);
                        egui::CentralPanel::default()
                            .frame(egui::Frame::new().fill(Color32::BLACK).inner_margin(Margin::same(8)))
                            .show(vctx, |ui| viewer_body(ui, id2, &mut v, &voice2, &mixer2, None));
                        if vctx.input(|i| i.viewport().close_requested()) {
                            v.out.close = true;
                        }
                        // 点了什么：让主窗口尽快处理
                        if v.out.any() {
                            vctx.request_repaint_of(egui::ViewportId::ROOT);
                        }
                    },
                );
                v = va.lock().unwrap();
            } else if std::mem::take(&mut v.embedded) {
                // 这一帧已经嵌在语音房间里画过了（第 59 项），不再浮窗口；点了什么照常在下面处理
            } else {
                *v.view.repaint_target.lock().unwrap() = None;
                refresh_viewer(ctx, *id, &mut v);
                let mut open = true;
                egui::Window::new(RichText::new(format!("📺 {name} 的屏幕")).strong())
                    .id(egui::Id::unique(("viewer", *id)))
                    .open(&mut open)
                    .default_size([960.0, 580.0])
                    .resizable(true)
                    .frame(egui::Frame::window(&ctx.global_style()).fill(Color32::BLACK).inner_margin(Margin::same(8)))
                    .show(ctx, |ui| viewer_body(ui, *id, &mut v, &voice, &mixer, None));
                if !open {
                    v.out.close = true;
                }
            }
            let out = std::mem::take(&mut v.out);
            if out.relayer {
                relayer.push((*id, v.low, v.route));
            }
            if let Some(sm) = out.smooth {
                smooth_change = Some(sm);
            }
            if let Some(b) = out.buffer {
                buffer_change = Some(b);
            }
            if out.go_full {
                go_full = Some(*id);
            }
            if out.toggle_pop {
                v.popped = !v.popped;
            }
            if out.close {
                close.push(*id);
            }
        }
        if let Some(sm) = smooth_change {
            self.cfg.smooth_playback = sm;
            video::SMOOTH_PLAYBACK.store(sm, Ordering::Relaxed);
        }
        if let Some((m, ms)) = buffer_change {
            self.cfg.view_buffer_mode = m;
            self.cfg.view_buffer_max_ms = ms;
        }
        for (id, low, route) in relayer {
            s.send(ClientMsg::Watch { target: id, on: true, low, route, cursor: true });
        }
        for id in close {
            self.views.remove(&id);
            crate::nv12view::forget(id);
            self.voice.video.watch(id, false);
            s.send(ClientMsg::Watch { target: id, on: false, low: false, route: 0, cursor: false });
        }
        if let Some(id) = go_full {
            self.fullscreen = Some(id);
            self.fs_mouse_at = std::time::Instant::now();
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
        }
        // 对方停止共享（观看窗口没了）也要退出全屏
        if exit_full || self.fullscreen.is_some_and(|id| !self.views.contains_key(&id)) {
            if self.fullscreen.take().is_some() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
            }
        }
    }

    /// 伴奏窗口：选一个正在出声的程序，把它的声音放给频道里的人
    fn ui_music(&mut self, ctx: &egui::Context) {
        if !self.music_window {
            return;
        }
        let in_channel = matches!(&self.screen, Screen::Main(s) if s.me().is_some_and(|u| u.channel_id.is_some()));
        let mut open = true;
        let mut start: Option<(u32, String)> = None;
        let mut stop = false;
        let mut monitor: Option<f32> = None;
        let mut refresh = false;
        egui::Window::new(RichText::new("🎵 伴奏").strong())
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .default_width(380.0)
            .show(ctx, |ui| {
                if !crate::loopback::supported() {
                    ui.label(RichText::new("伴奏需要 Windows 11（Windows 10 build 20348 以上）。").color(th::YELLOW()));
                    return;
                }
                if !in_channel {
                    ui.label(RichText::new("进入语音频道后才能放伴奏。").color(th::TEXT_DIM()));
                    return;
                }
                if let Some(m) = &self.music {
                    ui.label(format!("正在把「{}」的声音放给频道里的人", m.name));
                    ui.add_space(6.0);
                    let mut v = m.monitor * 100.0;
                    if ui.add(egui::Slider::new(&mut v, 5.0..=100.0).suffix("%").text("我这边听到的音量")).changed() {
                        monitor = Some(v / 100.0);
                    }
                    ui.label(
                        RichText::new("只影响你自己听到的大小，别人听到的不变；每个人也可以右键你的名字单独调。")
                            .size(11.5)
                            .color(th::TEXT_FAINT()),
                    );
                    ui.add_space(8.0);
                    if ui.button("⏹ 停止伴奏").clicked() {
                        stop = true;
                    }
                } else {
                    ui.label(RichText::new("选一个正在出声的程序（音乐播放器等），把它的声音放给频道里的人。不经过麦克风，闭麦也照放。").size(12.0).color(th::TEXT_DIM()));
                    ui.add_space(6.0);
                    if ui.button("🔄 刷新程序列表").clicked() {
                        refresh = true;
                    }
                    ui.add_space(4.0);
                    if self.music_apps.is_empty() {
                        ui.label(RichText::new("没有找到出声的程序：先让程序播放一下，再点刷新。").size(12.0).color(th::TEXT_FAINT()));
                    }
                    for (pid, name) in &self.music_apps {
                        if ui.add(egui::Button::new(format!("▶ {name}")).min_size(Vec2::new(ui.available_width(), 26.0))).clicked() {
                            start = Some((*pid, name.clone()));
                        }
                    }
                }
                if let Some(e) = &self.music_error {
                    ui.label(RichText::new(e).color(th::RED()));
                }
            });
        if refresh {
            self.music_apps = crate::loopback::audio_apps();
        }
        if let Some((pid, name)) = start {
            self.start_music(pid, name);
        }
        if stop {
            self.stop_music();
        }
        if let Some(v) = monitor {
            self.set_music_monitor(v);
        }
        if !open {
            self.music_window = false;
        }
    }
}

// ============================================================ 行组件

/// 频道行：🔊 名称 + 人数；当前频道左侧有强调色竖条
fn channel_row(ui: &mut egui::Ui, name: &str, count: usize, here: bool, full: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 32.0), Sense::click());
    let p = ui.painter();
    let bg = if here {
        th::BG_ACTIVE()
    } else if resp.hovered() && !full {
        th::BG_HOVER()
    } else {
        Color32::TRANSPARENT
    };
    p.rect_filled(rect, CornerRadius::same(8), bg);
    if here {
        let bar = egui::Rect::from_min_size(rect.min + Vec2::new(0.0, 7.0), Vec2::new(3.0, rect.height() - 14.0));
        p.rect_filled(bar, CornerRadius::same(2), th::ACCENT());
    }
    let fg = if here {
        th::TEXT()
    } else if full {
        th::TEXT_FAINT()
    } else {
        th::TEXT_DIM()
    };
    crate::icons::paint(p, egui::Rect::from_center_size(rect.left_center() + Vec2::new(20.0, 0.0), Vec2::splat(16.0)), Icon::Speaker, fg);
    p.text(rect.left_center() + Vec2::new(34.0, 0.0), Align2::LEFT_CENTER, name, egui::FontId::proportional(14.5), fg);
    if count > 0 {
        let txt = format!("{count}/{}", voc_proto::MAX_CHANNEL_USERS);
        let c = if full { th::RED() } else { th::TEXT_FAINT() };
        p.text(rect.right_center() - Vec2::new(10.0, 0.0), Align2::RIGHT_CENTER, txt, egui::FontId::proportional(12.0), c);
    }
    let tip = if here {
        "当前所在频道"
    } else if full {
        "频道已满"
    } else {
        "点击加入"
    };
    resp.on_hover_text(tip)
}

/// 成员行：头像 + 名字，右侧状态由 `right` 从右往左绘制；返回整行的响应（用于右键菜单）
fn member_row(ui: &mut egui::Ui, u: &UserInfo, speaking: bool, right: impl FnOnce(&mut egui::Ui)) -> egui::Response {
    let row = ui.horizontal(|ui| {
        ui.set_min_height(30.0);
        ui.add_space(14.0);
        th::avatar(ui, &u.name, 26.0, speaking);
        let name = RichText::new(&u.name).color(if speaking { th::TEXT() } else { th::TEXT_DIM() });
        ui.label(if speaking { name.strong() } else { name });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            right(ui);
        });
    });
    let resp = row.response.interact(Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(row.response.rect.expand(2.0), CornerRadius::same(8), th::BG_HOVER().gamma_multiply(0.35));
    }
    resp
}

fn device_combo(ui: &mut egui::Ui, id: &str, sel: &mut Option<String>, list: &[String]) -> bool {
    device_combo_w(ui, id, sel, list, 330.0)
}

fn device_combo_w(ui: &mut egui::Ui, id: &str, sel: &mut Option<String>, list: &[String], width: f32) -> bool {
    let before = sel.clone();
    egui::ComboBox::from_id_salt(id)
        .width(width)
        .selected_text(sel.clone().unwrap_or_else(|| "系统默认".into()))
        .show_ui(ui, |ui| {
            ui.selectable_value(sel, None, "系统默认");
            for d in list {
                ui.selectable_value(sel, Some(d.clone()), d);
            }
        });
    *sel != before
}

fn level_meter(ui: &mut egui::Ui, level_db: f32, thr_db: f32, active: bool) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 12.0), Sense::hover());
    let p = ui.painter();
    let to_x = |db: f32| rect.left() + rect.width() * ((db + 80.0) / 80.0).clamp(0.0, 1.0);
    p.rect_filled(rect, CornerRadius::same(6), th::BG_BASE());
    let fill = egui::Rect::from_min_max(rect.min, egui::pos2(to_x(level_db), rect.max.y));
    p.rect_filled(fill, CornerRadius::same(6), if active { th::GREEN() } else { th::BG_ACTIVE() });
    let x = to_x(thr_db);
    p.line_segment([egui::pos2(x, rect.top() - 3.0), egui::pos2(x, rect.bottom() + 3.0)], Stroke::new(2.0, th::TEXT()));
}

// ============================================================ eframe 入口

impl eframe::App for App {
    /// 窗口隐藏（缩到托盘）时 eframe 不调用 ui()，只调用 logic()：网络和托盘都放这里
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.background(ctx);
    }

    fn clear_color(&self, v: &egui::Visuals) -> [f32; 4] {
        if self.glass_window {
            [0.0; 4] // 云母窗口：透明底，透出系统背景
        } else {
            v.panel_fill.to_normalized_gamma_f32()
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        th::sync(ui.ctx(), self.cfg.theme, self.cfg.palette, self.glass_window);
        if self.glass_window {
            // 窗口移动、改大小、换深浅色后重新设一次云母（第 76 项）
            let vp = ui.ctx().input(|i| i.viewport().outer_rect);
            let key = (vp.map(|r| [r.min.x as i32, r.min.y as i32, r.max.x as i32, r.max.y as i32]), !th::is_light());
            if self.mica_key != Some(key) {
                let first = self.mica_key.is_none();
                self.mica_key = Some(key);
                let ok = crate::glass::hwnd_of(frame).is_some_and(|h| crate::glass::apply_mica(h, key.1));
                if first {
                    crate::applog::log(format!("毛玻璃：云母背景{}", if ok { "已打开" } else { "打不开" }));
                }
            }
        }
        // 闭麦 / 静音快捷键由后台线程全局检测（hotkey.rs），窗口在不在前台都一样
        self.handle_hotkeys();
        self.sync_user_volumes();
        self.auto_zoom(ui.ctx());

        match &self.screen {
            Screen::Login { error } => {
                let e = error.clone();
                self.ui_login(ui, e, false);
            }
            Screen::Connecting(_) => self.ui_login(ui, None, true),
            Screen::Main(_) if self.mini => self.ui_mini(ui),
            Screen::Main(_) => self.ui_main(ui),
            Screen::Reconnecting(_) => self.ui_reconnecting(ui),
        }
        let ctx = ui.ctx().clone();
        // 毛玻璃（第 59 项）：弹框出现前先截一帧（这一帧不画弹框），模糊后垫在弹框下面
        let modal = self.show_settings || self.show_share || self.music_window;
        let mut hide_modals = false;
        if !self.cfg.glass || !modal {
            self.glass_backdrop = None;
            self.glass_pending = None;
        } else if self.glass_backdrop.is_none() {
            let shot = self.glass_shot.lock().unwrap().take();
            match (shot, self.glass_pending) {
                (Some(img), _) => {
                    let tex = ctx.load_texture("glass-backdrop", img, egui::TextureOptions::LINEAR);
                    self.glass_backdrop = Some(crate::glass::Backdrop { tex });
                    self.glass_pending = None;
                }
                (None, None) => {
                    let (slot, c) = (self.glass_shot.clone(), ctx.clone());
                    ctx.request_screenshot(move |img| {
                        *slot.lock().unwrap() = Some(crate::glass::blur(&img));
                        c.request_repaint();
                    });
                    self.glass_pending = Some(std::time::Instant::now());
                    hide_modals = true;
                }
                // 等截图回来（最多 0.3 秒，等不到就不垫了，照常画弹框）
                (None, Some(t)) => hide_modals = t.elapsed() < std::time::Duration::from_millis(300),
            }
            if hide_modals {
                ctx.request_repaint();
            }
        }
        if let Some(b) = &self.glass_backdrop {
            // 背景层里另起一层：画在左右栏、聊天区上面，弹框（中间层）下面
            let p = ctx.layer_painter(egui::LayerId::new(egui::Order::Background, egui::Id::unique("glass-backdrop")));
            let r = ctx.content_rect();
            p.image(b.tex.id(), r, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
            let tint = th::p().bg_base;
            p.rect_filled(r, CornerRadius::ZERO, Color32::from_rgba_unmultiplied(tint.r(), tint.g(), tint.b(), if th::is_light() { 70 } else { 90 }));
        }
        if !hide_modals {
            self.ui_settings(&ctx);
            self.ui_share(&ctx);
        }
        self.ui_viewers(&ctx);
        self.ui_image_view(&ctx);
        if !hide_modals {
            self.ui_music(&ctx);
        }
        self.demo_screenshot(&ctx);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        if self.demo.active || self.test_mode {
            return; // 演示 / 测试模式不覆盖真实配置
        }
        if self.cfg.remember_code {
            self.cfg.code = self.code_input.trim().to_string();
        } else {
            self.cfg.code.clear();
            for s in &mut self.cfg.servers {
                s.code.clear();
            }
        }
        // 身份种子、授权码用 Windows 凭据保护加密后再写盘（第 41-6 项）
        let plain = self.cfg.seal_secrets();
        eframe::set_value(storage, eframe::APP_KEY, &self.cfg);
        self.cfg.restore_secrets(plain);
    }
}

/// 在观看画面上画共享者单独发来的鼠标（0.6 秒没收到就不画：共享者那边把鼠标画进画面了）
/// 共享声音走哪条线路（第 54 项；共享者统一选，和画面的线路分开）
/// 和我在同一个语音频道、解不了 AV1 的人（第 61 项；旧版本不报解码能力的也算解不了）
fn cannot_av1(s: &crate::app::Session) -> Vec<String> {
    let my_ch = s.users.iter().find(|u| u.id == s.user_id).and_then(|u| u.channel_id);
    s.users
        .iter()
        .filter(|u| u.id != s.user_id && my_ch.is_some() && u.channel_id == my_ch && !u.decoders.iter().any(|d| d == "av1"))
        .map(|u| u.name.clone())
        .collect()
}

fn share_audio_route_ui(ui: &mut egui::Ui, route: &mut u8, p2p_only: bool) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("共享声音线路").size(12.5).color(th::TEXT_DIM()));
        ui.add_enabled_ui(!p2p_only, |ui| {
            ui.selectable_value(route, crate::voice::SHARE_AUDIO_AUTO, "自动")
                .on_hover_text("跟你和他的语音走同一条线路：语音判成不稳、改走中转时声音也跟着改；直连有一点丢包时两条路同时发（推荐）");
            ui.selectable_value(route, crate::voice::SHARE_AUDIO_SERVER, "服务器")
                .on_hover_text("共享声音一律经服务器转发：多一点延迟，但不受直连丢包影响。占服务器带宽（每个观看者约 70 kbps）");
            ui.selectable_value(route, crate::voice::SHARE_AUDIO_DIRECT, "直连")
                .on_hover_text("能打通直连的人一律直连发，不看直连稳不稳；打不通的人还是经服务器");
        })
        .response
        .on_disabled_hover_text("你选了「只通过 P2P 直连发送」：共享声音也只走直连");
    });
    if !crate::voice::SERVER_SHARE_AUDIO_ROUTE.load(Ordering::Relaxed) && *route != crate::voice::SHARE_AUDIO_AUTO && !p2p_only {
        ui.label(RichText::new("服务器版本较旧：要升级到 0.6.2 这个选项才生效，现在共享声音跟着画面的线路走").size(11.5).color(th::YELLOW()));
    }
}

/// 取解码好的最新一帧；换上了新的一帧就记下时间（实际显示帧数，第 56-a 项）
fn refresh_viewer(ctx: &egui::Context, id: u32, v: &mut crate::app::Viewer) {
    let now = std::time::Instant::now();
    let fresh = {
        let f = v.view.frame.lock().unwrap();
        match f.as_ref() {
            Some(f) if f.seq != v.seq => {
                match &f.nv12 {
                    // 显卡上转颜色：只记下这一帧，画的时候交给着色器
                    Some(d) => v.gpu = Some(crate::nv12view::Nv12 { width: f.width as u32, height: f.height as u32, data: d.clone(), seq: f.seq }),
                    None => {
                        v.gpu = None;
                        let img = egui::ColorImage::from_rgba_unmultiplied([f.width, f.height], &f.rgba);
                        match &mut v.tex {
                            Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                            None => v.tex = Some(ctx.load_texture(format!("screen-{id}"), img, egui::TextureOptions::LINEAR)),
                        }
                    }
                }
                v.seq = f.seq;
                v.size = (f.width, f.height);
                Some(now.duration_since(f.at).as_secs_f32() * 1000.0)
            }
            _ => None,
        }
    };
    if let Some(wait) = fresh {
        v.shown.push_back(now);
        v.paint_ms = if v.paint_ms == 0.0 { wait } else { v.paint_ms * 0.9 + wait * 0.1 };
    }
    while v.shown.front().is_some_and(|t| now.duration_since(*t) > std::time::Duration::from_secs(1)) {
        v.shown.pop_front();
    }
}

/// 观看窗口的内容（窗口里、弹出的独立窗口里共用）。点了什么写进 v.out，主循环处理
/// 共享声音跟着画面的缓冲晚一点（第 78 项，音画同步）：画面比到达多等了 W ms，声音自己的抖动缓冲已经晚了 B ms，
/// 再额外加 W − B（不够就不加）。变化不到 5 ms 不改，免得来回抖
fn sync_share_audio(mixer: &Arc<std::sync::Mutex<crate::audio::Mixer>>, id: u32, view: &crate::video::View) {
    let sid = crate::audio::stream_mixer_id(voc_proto::udp::SHARE_AUDIO, id);
    let wait = view.av_wait_ms.load(Ordering::Relaxed) as f32;
    let mut m = mixer.lock().unwrap();
    let want = if wait > 0.0 { (wait - m.base_buffer_ms(sid).unwrap_or(70.0)).max(0.0) } else { 0.0 };
    let cur = m.extra_delay(sid);
    if (want - cur).abs() >= 5.0 || (want == 0.0 && cur > 0.0) {
        m.set_extra_delay(sid, want);
    }
}

/// img_h：嵌在语音房间里时画面区域的高度（None = 占满剩下的地方：弹出窗口、全屏）
fn viewer_body(
    ui: &mut egui::Ui,
    id: u32,
    v: &mut crate::app::Viewer,
    voice: &Arc<crate::voice::VoiceState>,
    mixer: &Arc<std::sync::Mutex<crate::audio::Mixer>>,
    img_h: Option<f32>,
) {
    // 弹出的窗口单独刷新：这里也跟一下共享声音的额外缓冲（第 78 项）
    sync_share_audio(mixer, id, &v.view);
    let info = v.info.clone();
    let desc = info.desc.clone();
    let (shown_fps, jitter) = v.shown_stats();
    // 每秒更新一次这一秒的收包情况（掉帧原因用）
    if let Some((now_st, _, _)) = voice.video.rx_stats(id) {
        match v.rx_prev {
            Some((prev, at)) if at.elapsed() >= std::time::Duration::from_secs(1) => {
                let lost = now_st.data_lost.saturating_sub(prev.data_lost) as f32;
                let total = now_st.data_total.saturating_sub(prev.data_total).max(1) as f32;
                v.rates = th::ViewRates {
                    rx_fps: v.view.rx_fps.load(Ordering::Relaxed),
                    // 实际换上屏幕的帧数（不是解码交出的帧数，第 56-a 项）
                    shown_fps,
                    loss_pct: lost * 100.0 / total,
                    gaps: now_st.gaps.saturating_sub(prev.gaps),
                    fec: now_st.fec_recovered.saturating_sub(prev.fec_recovered),
                    rtx: now_st.rtx_recovered.saturating_sub(prev.rtx_recovered),
                };
                v.rx_prev = Some((now_st, std::time::Instant::now()));
            }
            None => v.rx_prev = Some((now_st, std::time::Instant::now())),
            _ => {}
        }
    }
    let sinfo = v.view.sharer_info.lock().unwrap().filter(|(_, t)| t.elapsed() < std::time::Duration::from_secs(3)).map(|(i, _)| i);
    let why = th::drop_reason(sinfo.as_ref(), &v.rates);
    let (fg, bg) = if why.is_some() { (th::YELLOW(), th::BG_ELEVATED()) } else { (th::TEXT_DIM(), th::BG_ELEVATED()) };
    let mut tip = String::new();
    if let Some(i) = &sinfo {
        tip.push_str(&format!(
            "共享者：选了 {} 帧 · 屏幕 / 游戏给了 {} 帧 · 采集 {} 帧 · 编码发出 {} 帧\n  画面没变少发 {} 帧 · 码率控制跳过 {} 帧 · 每帧编码 {} ms · 码率 {:.1} Mbps（设定值的 {}%）{}\n",
            i.target_fps, i.source_fps, i.capture_fps, i.sent_fps, i.static_skipped, i.rate_skipped, i.encode_ms,
            i.kbps as f32 / 1000.0, i.rate_pct, if i.congested { " · 上行拥塞" } else { "" }
        ));
    } else {
        tip.push_str("共享者是旧版本，看不到他那边的采集 / 编码帧数\n");
    }
    let r = v.rates;
    tip.push_str(&format!(
        "你这边：收到 {} 帧 · 解码交出 {} 帧 · 实际显示 {} 帧 · 画面间隔抖动 {:.0} ms\n  原始丢包 {:.1}% · 冗余救回 {} 片 · 补发收回 {} 片 · 放弃 {} 帧",
        r.rx_fps,
        v.view.fps.load(Ordering::Relaxed),
        shown_fps,
        jitter,
        r.loss_pct,
        r.fec,
        r.rtx,
        r.gaps
    ));
    // 大概的延迟：共享者编码 + 单程网络（往返的一半）+ 本机解码 + 等着画上屏幕（第 36-6 / 56-a 项）
    let rtt = voice.video.rx_stats(id).map(|s| s.2).unwrap_or(0);
    let show = v.view.show_ms.load(Ordering::Relaxed);
    let paint = v.paint_ms.round() as u32;
    let buf_wait = v.view.av_wait_ms.load(Ordering::Relaxed);
    let lat = sinfo.map(|i| i.encode_ms as u32).unwrap_or(0) + rtt / 2 + show + paint + buf_wait;
    tip.push_str(&format!(
        "\n延迟大约 {lat} ms（共享者编码 + 网络单程约 {} ms + 缓冲 {buf_wait} ms + 本机解码 {show} ms + 等着画上屏幕 {paint} ms；不含截屏和显示器本身）",
        rtt / 2
    ));
    if let Some(w) = &why {
        tip = format!("掉帧原因：{w}\n\n{tip}");
    }
    // 画面间隔忽长忽短（抖动 > 8 ms）时也标黄：平均帧数够，看着也会一顿一顿
    let (fg, bg) = if jitter > 8.0 && why.is_none() { (th::YELLOW(), th::BG_ELEVATED()) } else { (fg, bg) };
    let dec = v.view.decoder.lock().unwrap().clone();
    let route_now = voice.video_direct(id);
    let route_txt = match route_now {
        Some(true) => "直连",
        // 经别的观看者转来的（只走 P2P 时不会经服务器）
        Some(false) if info.p2p_only => "经队友转发",
        Some(false) => "经服务器",
        None => "没收到画面",
    };
    let route_tip = th::video_route_explain(route_now, voice.current_link().and_then(|l| l.direct_peers().get(&id).copied()).as_ref(), v.route, info.p2p_only);
    let full_tip = format!(
        "{tip}\n\n分辨率 {}×{} · 解码 {} · 🔒 端到端加密 · 画面线路：{route_txt}{}",
        v.size.0,
        v.size.1,
        if dec.is_empty() { "-" } else { dec.as_str() },
        if info.p2p_only { " · 共享者设置了只走 P2P" } else { "" }
    );
    // 标题栏（第 85 项）：左边是谁的屏幕 + 一个状态小标签（详细数字在悬停提示里），右边 声音 · ⚙ · 弹出 · 全屏 · 关闭
    ui.horizontal(|ui| {
        let title = if info.name.is_empty() { "屏幕共享".to_string() } else { format!("{} 的屏幕", info.name) };
        ui.label(RichText::new(title).strong());
        th::pill(ui, format!("{desc} · {shown_fps} fps · 约 {lat} ms"), fg, bg).on_hover_text(full_tip);
        if route_now.is_none() {
            th::pill(ui, "没收到画面", th::YELLOW(), th::BG_ELEVATED()).on_hover_text(route_tip.clone());
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            if th::icon_button(ui, Icon::Close, "关闭画面（不看了）", Tone::Normal).clicked() {
                v.out.close = true;
            }
            if th::icon_button(ui, Icon::Fullscreen, "全屏观看（双击画面也可以；Esc 退出）", Tone::Normal).clicked() {
                v.out.go_full = true;
            }
            let pop_tip = if v.popped { "收回到 VOC 主窗口里" } else { "弹出成独立窗口：可以拖到另一个显示器、置顶，主窗口照常聊天" };
            if th::icon_button(ui, if v.popped { Icon::Back } else { Icon::PopOut }, pop_tip, Tone::Normal).clicked() {
                v.out.toggle_pop = true;
            }
            if v.popped && ui.checkbox(&mut v.on_top, "置顶").on_hover_text("这个窗口总在最前面（打游戏时也看得到）").changed() {
                let level = if v.on_top { egui::WindowLevel::AlwaysOnTop } else { egui::WindowLevel::Normal };
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::WindowLevel(level));
            }
            // 画质 / 线路 / 播放方式 / 缓冲收进「⚙」菜单（第 53-4 项）；线路状态也写在最上面
            ui.menu_button(RichText::new("⚙").size(16.0), |ui| {
                ui.set_min_width(300.0);
                ui.label(RichText::new(format!("画面线路：{route_txt}")).size(12.0).color(th::TEXT())).on_hover_text(route_tip.clone());
                if info.p2p_only {
                    ui.label(RichText::new("共享者设置了只走 P2P（画面不经服务器）").size(11.5).color(th::TEXT_FAINT()));
                }
                ui.separator();

                    if info.simulcast {
                        ui.label(RichText::new("画质").size(12.0).color(th::TEXT_DIM()));
                        let before = v.low;
                        ui.horizontal(|ui| {
                            ui.selectable_value(&mut v.low, false, "原画").on_hover_text(format!("按共享者设置的 {desc} 观看"));
                            ui.selectable_value(&mut v.low, true, "流畅").on_hover_text("360P 低画质，省流量、网络差时更不容易卡");
                        });
                        if v.low != before {
                            v.out.relayer = true;
                        }
                        ui.separator();
                    }
                    ui.label(RichText::new("画面线路").size(12.0).color(th::TEXT_DIM()));
                    let before = v.route;
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut v.route, 0, "自动").on_hover_text("能直连就直连，否则经服务器（推荐）");
                        ui.selectable_value(&mut v.route, 1, "P2P").on_hover_text(
                            "画面直接从共享者发给你，不受对方上行预算限制；和对方打不通时仍经服务器。\n双方都要 0.4.2 以上，服务器也要升级",
                        );
                        ui.selectable_value(&mut v.route, 2, "服务器")
                            .on_hover_text("画面经服务器转给你。直连卡顿、花屏或一直等待画面时试试\n双方都要 0.4.2 以上，服务器也要升级");
                    });
                    if v.route != before {
                        v.out.relayer = true;
                    }
                    // 播放方式：流畅优先（匀速，默认）/ 延迟优先（到了就显示）
                    ui.separator();
                    ui.label(RichText::new("播放方式").size(12.0).color(th::TEXT_DIM()));
                    let mut smooth = v.view.smooth.load(Ordering::Relaxed);
                    let was = smooth;
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut smooth, true, "流畅优先")
                            .on_hover_text("画面按共享者截屏时的节奏匀速显示，看着更顺；网络稳时只多一两帧的延迟");
                        ui.selectable_value(&mut smooth, false, "延迟优先").on_hover_text("画面一到就显示，延迟最低；网络抖时会一顿一顿的");
                    });
                    if smooth != was {
                        v.view.smooth.store(smooth, Ordering::Relaxed);
                        v.out.smooth = Some(smooth);
                    }
                    // 观看缓冲（第 78 项）：先攒一点再放，网络停一下画面不停；共享声音跟着一起晚，音画对齐
                    ui.separator();
                    ui.label(RichText::new("缓冲（防卡顿）").size(12.0).color(th::TEXT_DIM()));
                    let mut mode = video::BUFFER_MODE.load(Ordering::Relaxed);
                    let mut max_ms = video::BUFFER_MAX_MS.load(Ordering::Relaxed);
                    let (m0, x0) = (mode, max_ms);
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut mode, 0, "关").on_hover_text("不加缓冲，延迟最低（默认）");
                        ui.selectable_value(&mut mode, 1, "自动")
                            .on_hover_text("按你这边的网络自己调：卡过就加一点，稳了慢慢减；不超过下面设的上限");
                        ui.selectable_value(&mut mode, 2, "固定").on_hover_text("一直缓冲下面设的这么多");
                    });
                    let label = if mode == 2 { "缓冲" } else { "最多缓冲" };
                    ui.add_enabled(
                        mode != 0,
                        egui::Slider::new(&mut max_ms, video::BUFFER_RANGE_MS.0..=video::BUFFER_RANGE_MS.1).suffix(" ms").step_by(10.0).text(label),
                    );
                    if mode != 0 {
                        let now_ms = v.view.buffer_ms.load(Ordering::Relaxed);
                        ui.label(
                            RichText::new(format!("现在缓冲 {now_ms} ms：网络停顿不超过这么久，画面照常走；画面会晚这么多，共享声音跟着一起晚（音画同步）"))
                                .size(11.5)
                                .color(th::TEXT_FAINT()),
                        );
                        ui.label(RichText::new("开了缓冲时不用上面的「播放方式」").size(11.5).color(th::TEXT_FAINT()));
                    }
                    if (mode, max_ms) != (m0, x0) {
                        video::BUFFER_MODE.store(mode, Ordering::Relaxed);
                        video::BUFFER_MAX_MS.store(max_ms, Ordering::Relaxed);
                        v.out.buffer = Some((mode, max_ms));
                    }
            })
            .response
            .on_hover_text("画质 · 线路 · 播放方式 · 缓冲");
            // 共享声音：点开调音量（只影响你自己听到的，和他说话的音量分开）
            let sid = crate::audio::stream_mixer_id(voc_proto::udp::SHARE_AUDIO, id);
            let on = info.share_audio;
            let vol_now = mixer.lock().unwrap().volume(sid);
            let icon = if !on || vol_now < 0.005 { "🔇" } else { "🔊" };
            ui.add_enabled_ui(on, |ui| {
                ui.menu_button(RichText::new(icon).size(15.0), |ui| {
                    ui.set_min_width(260.0);
                    let mut m = mixer.lock().unwrap();

                    // 共享声音的播放情况（第 54-d 项）：分清杂音是网络丢包、播放缓冲见底，还是共享者那边录音丢数据
                    let a = m.stream_stats(sid).unwrap_or_default();
                    let glitch = v.view.sharer_info.lock().unwrap().map(|(i, _)| i.audio_glitches).unwrap_or(0);
                    let audio_tip = format!(
                    "只调你听到的共享声音大小（游戏、视频的声音），不影响他说话的音量；右键他的名字也能调\n\n\
                    共享声音：缓冲 {} ms（目标 {}）· 丢包补音 {} 帧 · 播空 {} 次 · 积压丢弃 {} 次 · 播放速度 {:+.1}%\n\
                    音量太大、峰值被压低：{} 次\n\
                    共享者那边录音没跟上：最近一秒 {glitch} 次\n\
                    有「咔哒」声时：补音、播空多 = 网络问题；录音没跟上 = 共享者电脑太忙",
                    a.buffered_ms,
                    a.target_ms,
                    a.concealed,
                    a.underruns,
                    a.drops,
                    a.rate_permille as f32 / 10.0,
                    a.limited
                    );
                    ui.label(RichText::new("共享声音（游戏、视频的声音）").size(12.0).color(th::TEXT_DIM()));
                    let mut g = m.volume(sid);
                    if th::vol_slider(ui, &mut g, "", 180.0).on_hover_text(audio_tip).changed() {
                        m.set_volume(sid, g);
                    }
                    // 第 57 项：真的在压峰值时提示（不压就不显示）
                    if a.limited_recently && g > 1.0 {
                        ui.label(RichText::new("音量过大：响的地方正在被压低，调到 100% 左右更自然").size(11.5).color(th::YELLOW()));
                    }
                    let muted = g < 0.005;
                    if ui.button(if muted { "恢复共享声音" } else { "共享声音静音" }).clicked() {
                        m.set_volume(sid, if muted { 1.0 } else { 0.0 });
                    }
                })
                .response
                .on_hover_text("共享声音音量")
            })
            .response
            .on_disabled_hover_text("对方没有勾选「同时共享电脑声音」");
        });
    });

    // 共享者开了 120 帧、这台电脑的屏幕显示不了那么多（第 64 项）
    if let Some(t) = sinfo.map(|i| i.target_fps as u32).filter(|t| *t > 60) {
        let r = video::max_refresh();
        if r + 1 < t {
            ui.label(RichText::new(format!("对方在用 {t} 帧共享（试验），你的屏幕最多显示 {r} 帧")).size(12.0).color(th::TEXT_FAINT()));
        }
    }
    // 共享的是窗口：最小化 / 关掉了（第 60-1 项）
    match sinfo.map(|i| i.window_state) {
        Some(1) => {
            ui.label(RichText::new("共享的窗口被最小化了，对方把它恢复后画面就会回来").size(12.0).color(th::YELLOW()));
        }
        Some(2) => {
            ui.label(RichText::new("共享的窗口已经关了").size(12.0).color(th::YELLOW()));
        }
        _ => {}
    }
    // 选项冲突时以更保守的一方为准，并说明原因（第 46 / 44 项）
    if info.p2p_only && voice.video_direct(id).is_none() {
        let why = if v.route == 2 {
            "共享者设置了「只走 P2P」，而你选了画面「服务器」：两边都按更保守的来，这样看不了。改选「自动」或「P2P」试试。"
        } else {
            "共享者设置了「只走 P2P」（画面不经服务器），你和他之间还没打通直连，也没有别的观看者能转给你。打洞成功后会自动出画面。"
        };
        ui.label(RichText::new(why).size(12.0).color(th::YELLOW()));
    }
    let mut full = false;
    let draw = |ui: &mut egui::Ui, v: &mut crate::app::Viewer, full: &mut bool| match (&v.gpu, &v.tex) {
        (Some(g), _) => {
            let avail = ui.available_size();
            let (w, h) = (v.size.0.max(1) as f32, v.size.1.max(1) as f32);
            let scale = (avail.x / w).min(avail.y / h).max(0.05);
            let (area, r) = ui.allocate_exact_size(avail, Sense::click());
            let img = egui::Rect::from_center_size(area.center(), Vec2::new(w * scale, h * scale));
            crate::nv12view::paint(ui.painter(), img, id, g.clone());
            draw_remote_cursor(ui.painter(), img, &v.view);
            *full = r.double_clicked();
        }
        (None, Some(t)) => {
            let avail = ui.available_size();
            let (w, h) = (v.size.0.max(1) as f32, v.size.1.max(1) as f32);
            let scale = (avail.x / w).min(avail.y / h).max(0.05);
            ui.centered_and_justified(|ui| {
                let r = ui.add(egui::Image::new(egui::load::SizedTexture::new(t.id(), Vec2::new(w * scale, h * scale))).sense(Sense::click()));
                draw_remote_cursor(ui.painter(), r.rect, &v.view);
                *full = r.double_clicked();
            });
        }
        (None, None) => {
            ui.centered_and_justified(|ui| ui.label(RichText::new("正在等待画面…").color(th::TEXT_DIM())));
        }
    };
    match img_h {
        Some(h) => {
            let size = Vec2::new(ui.available_width(), h);
            ui.allocate_ui_with_layout(size, Layout::top_down(Align::Center), |ui| {
                ui.set_min_size(size);
                draw(ui, v, &mut full);
            });
        }
        None => draw(ui, v, &mut full),
    }
    if full {
        v.out.go_full = true;
    }
}

fn draw_remote_cursor(p: &egui::Painter, img: egui::Rect, view: &crate::video::View) {
    use voc_proto::udp::*;
    let Some((x, y, shape, visible, at)) = *view.cursor.lock().unwrap() else { return };
    if !visible || at.elapsed() > std::time::Duration::from_millis(600) {
        return;
    }
    let pos = img.min + Vec2::new(x * img.width(), y * img.height());
    let k = (img.height() / 1080.0).clamp(0.6, 1.6); // 按画面大小缩放
    let black = Stroke::new(1.2 * k, Color32::BLACK);
    let white = Stroke::new(2.4 * k, Color32::WHITE);
    let line = |a: egui::Pos2, b: egui::Pos2| {
        p.line_segment([a, b], white);
        p.line_segment([a, b], black);
    };
    match shape {
        CURSOR_IBEAM => {
            let h = 9.0 * k;
            line(pos + Vec2::new(0.0, -h), pos + Vec2::new(0.0, h));
            line(pos + Vec2::new(-3.0 * k, -h), pos + Vec2::new(3.0 * k, -h));
            line(pos + Vec2::new(-3.0 * k, h), pos + Vec2::new(3.0 * k, h));
        }
        CURSOR_CROSS | CURSOR_SIZE_ALL => {
            let h = 8.0 * k;
            line(pos + Vec2::new(-h, 0.0), pos + Vec2::new(h, 0.0));
            line(pos + Vec2::new(0.0, -h), pos + Vec2::new(0.0, h));
        }
        CURSOR_SIZE_WE => line(pos + Vec2::new(-9.0 * k, 0.0), pos + Vec2::new(9.0 * k, 0.0)),
        CURSOR_SIZE_NS => line(pos + Vec2::new(0.0, -9.0 * k), pos + Vec2::new(0.0, 9.0 * k)),
        CURSOR_WAIT => {
            p.circle_stroke(pos, 7.0 * k, white);
            p.circle_stroke(pos, 7.0 * k, black);
        }
        _ => {
            // 箭头（手型也画箭头，再加个小圆点）
            let pts = [(0.0, 0.0), (0.0, 17.0), (4.5, 13.0), (8.0, 20.0), (10.5, 19.0), (7.0, 12.0), (12.5, 12.0)];
            let poly: Vec<egui::Pos2> = pts.iter().map(|(a, b)| pos + Vec2::new(*a * k, *b * k)).collect();
            p.add(egui::Shape::convex_polygon(poly.clone(), Color32::WHITE, Stroke::NONE));
            p.add(egui::Shape::closed_line(poly, Stroke::new(1.2 * k, Color32::BLACK)));
            if shape == CURSOR_HAND {
                p.circle_filled(pos + Vec2::new(14.0 * k, 4.0 * k), 3.0 * k, Color32::from_rgb(80, 160, 255));
            }
        }
    }
}

/// 界面缩放的基准窗口大小（第 84 项）：这么大 = 100%
const BASE_W: f32 = 1180.0;
const BASE_H: f32 = 760.0;
/// 按比例要缩到这以下就换迷你模式（用户定 50%）
const MINI_BELOW: f32 = 0.5;
