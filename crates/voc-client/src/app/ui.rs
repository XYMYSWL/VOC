//! 界面绘制（登录页、主界面、设置、屏幕共享、观看窗口）。逻辑在 `app.rs`，配色和组件在 `theme.rs`。

use super::*;
use crate::theme::{self as th, Tone};
use eframe::egui::{Align, Align2, Color32, CornerRadius, Layout, Margin, RichText, Sense, Stroke, Vec2};

impl App {
    // ============================================================ 登录页

    /// 网络断开、正在自动重连
    fn ui_reconnecting(&mut self, ui: &mut egui::Ui) {
        let Screen::Reconnecting(r) = &self.screen else { return };
        let (reason, attempt, busy) = (r.reason.clone(), r.attempt, r.pending.is_some());
        let wait = r.next_at.saturating_duration_since(std::time::Instant::now()).as_secs() + 1;
        let mut cancel = false;
        egui::CentralPanel::default().frame(egui::Frame::new().fill(th::BG_BASE)).show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() * 0.28).max(40.0));
                ui.label(RichText::new("与服务器的连接断开了").size(22.0).strong().color(th::TEXT));
                ui.add_space(6.0);
                ui.label(RichText::new(&reason).size(13.0).color(th::TEXT_DIM));
                ui.add_space(18.0);
                ui.horizontal(|ui| {
                    ui.add_space((ui.available_width() - 220.0).max(0.0) / 2.0);
                    ui.spinner();
                    let text = if busy {
                        format!("正在重连…（第 {} 次）", attempt + 1)
                    } else {
                        format!("{wait} 秒后重连（已失败 {attempt} 次）")
                    };
                    ui.label(RichText::new(text).color(th::TEXT));
                });
                ui.add_space(8.0);
                ui.label(RichText::new("连上后会自动回到原来的语音频道，聊天记录还在。").size(12.0).color(th::TEXT_FAINT));
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
        egui::CentralPanel::default().frame(egui::Frame::new().fill(th::BG_BASE)).show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space((ui.available_height() * 0.10).max(28.0));
                    ui.label(RichText::new("VOC").size(46.0).strong().color(th::ACCENT_HOVER));
                    ui.label(RichText::new("加密语音开黑 · 屏幕共享 · P2P 直连").size(14.0).color(th::TEXT_DIM));
                    match self.updater.get() {
                        crate::update::UpdateState::Available { version, url, .. } => {
                            ui.add_space(6.0);
                            if self.updater.can_auto_install() {
                                if ui.link(RichText::new(format!("⬆ 发现新版本 v{version}，点此一键更新")).color(th::ACCENT_HOVER)).clicked() {
                                    self.updater.install(ui.ctx().clone());
                                }
                            } else {
                                ui.hyperlink_to(RichText::new(format!("⬆ 发现新版本 v{version}，点此下载")).color(th::ACCENT_HOVER), url);
                            }
                        }
                        crate::update::UpdateState::Downloading { version, percent } => {
                            ui.add_space(6.0);
                            ui.label(RichText::new(format!("正在下载 v{version}… {percent}%")).color(th::TEXT_DIM));
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
                                            RichText::new(title.clone()).color(if active { Color32::WHITE } else { th::TEXT }),
                                        )
                                        .fill(if active { th::ACCENT_SOFT } else { th::BG_HOVER })
                                        .stroke(if active { Stroke::new(1.0, th::ACCENT) } else { Stroke::NONE })
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
                                ui.label(RichText::new(label).size(12.5).color(th::TEXT_DIM));
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
                                if th::icon_button(ui, eye, "显示 / 隐藏授权码", Tone::Normal).clicked() {
                                    self.show_code = !self.show_code;
                                }
                            });
                            let code_ok = voc_proto::auth::AuthCode::parse(&self.code_input);
                            if !self.code_input.trim().is_empty() {
                                if let Err(e) = &code_ok {
                                    ui.label(RichText::new(e).size(12.0).color(th::RED));
                                }
                            }
                            ui.checkbox(&mut self.cfg.remember_code, RichText::new("在本机记住授权码").color(th::TEXT_DIM));
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
                                    .fill(th::RED_SOFT)
                                    .corner_radius(CornerRadius::same(8))
                                    .inner_margin(Margin::symmetric(10, 8))
                                    .show(ui, |ui| {
                                        ui.set_width(ui.available_width());
                                        ui.label(RichText::new(e).color(th::RED));
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
                        RichText::new("🔒 ML-KEM-768 抗量子加密 · 端到端加密 · 流量随机化抗 DPI").size(12.0).color(th::TEXT_FAINT),
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

        // ---------------- 顶栏
        egui::Panel::top("top")
            .frame(egui::Frame::new().fill(th::BG_PANEL).inner_margin(Margin::symmetric(16, 10)).stroke(Stroke::new(1.0, th::BORDER)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&s.server_name).size(16.5).strong());
                    if let Some(ch) = my_channel.and_then(|c| s.channels.iter().find(|x| x.id == c)) {
                        ui.label(RichText::new("/").color(th::TEXT_FAINT));
                        ui.label(RichText::new(format!("🔊 {}", ch.name)).color(th::TEXT_DIM));
                    }
                    ui.add_space(8.0);
                    let transport = self.voice.transport_udp().or(self.demo.transport);
                    match transport {
                        Some(true) => th::pill(ui, "🔒 UDP 直连", th::GREEN, th::GREEN_SOFT)
                            .on_hover_text("ML-KEM-768 抗量子加密 + 端到端加密，流量随机化"),
                        Some(false) => th::pill(ui, "🔒 TCP 隧道", th::YELLOW, th::YELLOW_SOFT)
                            .on_hover_text("UDP 不通或被限速，语音暂时走加密 TCP；UDP 恢复后自动切回"),
                        None => th::pill(ui, "🔒 连接中", th::TEXT_DIM, th::BG_ELEVATED),
                    };
                    let ping = match self.voice.ping_ms.load(Ordering::Relaxed) {
                        0 => self.demo.ping,
                        p => p,
                    };
                    if ping > 0 {
                        let (fg, bg) = if ping < 80 {
                            (th::GREEN, th::GREEN_SOFT)
                        } else if ping < 200 {
                            (th::YELLOW, th::YELLOW_SOFT)
                        } else {
                            (th::RED, th::RED_SOFT)
                        };
                        th::pill(ui, format!("{ping} ms"), fg, bg).on_hover_text("到服务器的往返延迟");
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        match self.updater.get() {
                            crate::update::UpdateState::Available { version, url, .. } => {
                                let auto = self.updater.can_auto_install();
                                let b = egui::Button::new(RichText::new(format!("⬆ 新版本 v{version}")).size(12.0).color(Color32::WHITE))
                                    .fill(th::ACCENT)
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
                                th::pill(ui, format!("⬇ 更新 {percent}%"), th::TEXT, th::BG_HOVER);
                            }
                            _ => {}
                        }
                        if ui.button(RichText::new("断开").color(th::TEXT_DIM)).on_hover_text("断开连接").clicked() {
                            disconnect = true;
                        }
                        if th::icon_button(ui, "⚙", "设置", Tone::Normal).clicked() {
                            self.show_settings = !self.show_settings;
                        }
                    });
                });
            });

        // ---------------- 左侧：频道 + 自己
        egui::Panel::left("channels")
            .frame(egui::Frame::new().fill(th::BG_SIDEBAR).inner_margin(Margin::symmetric(10, 10)))
            .resizable(true)
            .default_size(290.0)
            .min_size(240.0)
            .show(ui, |ui| {
                // 底部：自己的状态卡
                egui::Panel::bottom("me")
                    .frame(
                        egui::Frame::new()
                            .fill(th::BG_BASE)
                            .corner_radius(CornerRadius::same(12))
                            .inner_margin(Margin::symmetric(10, 8)),
                    )
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let name = me.as_ref().map(|u| u.name.as_str()).unwrap_or("?");
                            th::avatar(ui, name, 36.0, me.as_ref().is_some_and(|u| speaking(u.id)));
                            ui.vertical(|ui| {
                                ui.add_space(1.0);
                                ui.label(RichText::new(name).strong());
                                let status = match (my_channel, &self.sharer) {
                                    (Some(_), Some(sh)) => format!("🖥 正在共享 {}", sh.cfg.desc()),
                                    (Some(c), None) => {
                                        format!("🔊 {}", s.channels.iter().find(|x| x.id == c).map(|x| x.name.as_str()).unwrap_or(""))
                                    }
                                    _ => "未进入语音频道".into(),
                                };
                                ui.label(RichText::new(status).size(12.0).color(th::TEXT_FAINT));
                            });
                        });
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            let muted = self.settings.muted.load(Ordering::Relaxed);
                            let deaf = self.settings.deafened.load(Ordering::Relaxed);
                            let (mk, dk) = (self.cfg.mute_key.label(), self.cfg.deafen_key.label());
                            let mt = if muted { format!("取消闭麦（{mk}）") } else { format!("闭麦（{mk}）") };
                            if th::icon_button(ui, "🎙", &mt, if muted { Tone::Danger } else { Tone::Normal }).clicked() {
                                self.settings.muted.store(!muted, Ordering::Relaxed);
                                toggled_state = true;
                            }
                            let (di, dt) = if deaf { ("🔇", format!("取消静音（{dk}）")) } else { ("🎧", format!("静音：听不到别人（{dk}）")) };
                            if th::icon_button(ui, di, &dt, if deaf { Tone::Danger } else { Tone::Normal }).clicked() {
                                self.settings.deafened.store(!deaf, Ordering::Relaxed);
                                toggled_state = true;
                            }
                            if my_channel.is_some() {
                                if self.sharer.is_some() {
                                    if th::icon_button(ui, "🖥", "共享中：点击查看详情 / 停止", Tone::Accent).clicked() {
                                        self.show_share = true;
                                    }
                                } else if th::icon_button(ui, "🖥", "共享屏幕", Tone::Normal).clicked() {
                                    self.show_share = true;
                                }
                                let tone = if self.music.is_some() { Tone::Accent } else { Tone::Normal };
                                if th::icon_button(ui, "🎵", "伴奏：把某个程序的声音放给频道里的人", tone).clicked() {
                                    self.music_window = !self.music_window;
                                    if self.music_window {
                                        self.music_apps = crate::loopback::audio_apps();
                                    }
                                }
                            }
                            if th::icon_button(ui, "⚙", "设置", Tone::Normal).clicked() {
                                self.show_settings = !self.show_settings;
                            }
                            if my_channel.is_some() {
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if th::icon_button(ui, "📞", "退出语音频道", Tone::Danger).clicked() {
                                        s.send(ClientMsg::LeaveChannel);
                                    }
                                });
                            }
                        });
                    });

                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        th::section(ui, "语音频道");
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui
                                .add(egui::Button::new(RichText::new("＋").color(th::TEXT_DIM)).frame(false))
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
                                    th::pill(ui, "静音", th::RED, th::RED_SOFT).on_hover_text("听不到别人");
                                } else if u.muted {
                                    th::pill(ui, "闭麦", th::RED, th::RED_SOFT);
                                }
                                if !is_me && matches!(info, Some((Trust::Changed, _, _))) {
                                    ui.label(RichText::new("⚠").color(th::RED))
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
                                        th::pill(ui, "🔒 不透露 IP（手动）", th::TEXT_DIM, th::BG_ELEVATED)
                                            .on_hover_text("你选了不和他直连、互相不透露 IP：语音、画面都经服务器（语音可以经队友中转）。右键他的名字可以改");
                                    } else if pref.voice == voice::VoiceRoute::Server {
                                        th::pill(ui, "⇄ 语音走服务器（手动）", th::TEXT_DIM, th::BG_ELEVATED)
                                            .on_hover_text("你选了和他之间的语音走服务器（画面照常）；右键他的名字可以改回自动");
                                    } else if fallback {
                                        th::pill(ui, "⇄ 语音暂走服务器", th::YELLOW, th::BG_ELEVATED)
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
                                        th::pill(ui, "手动直连", th::ACCENT_HOVER, th::ACCENT_SOFT)
                                            .on_hover_text(format!("你选了：{}。打通了就一直直连；打不通时仍走服务器。右键可以改回自动", pref.label()));
                                    }
                                    if let voice::VoiceRoute::Via(n) = &pref.voice {
                                        if !pref.hide_ip && lnk.as_ref().is_some_and(|l| l.manual_via_unmet(u.id).is_some()) {
                                            th::pill(ui, format!("经 {n} 中转暂不可用"), th::YELLOW, th::BG_ELEVATED).on_hover_text(format!(
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
                                        th::pill(ui, label, th::GREEN, th::GREEN_SOFT).on_hover_text(format!(
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
                                        th::pill(ui, format!("🎵 {name}"), th::ACCENT_HOVER, th::ACCENT_SOFT).on_hover_text(tip);
                                    }
                                    if let Some(desc) = &u.sharing {
                                        if is_me {
                                            th::pill(ui, format!("🖥 共享中 {desc}"), th::ACCENT_HOVER, th::ACCENT_SOFT);
                                        } else if here {
                                            let watching = self.views.contains_key(&u.id);
                                            let (txt, fg, bg) = if watching {
                                                ("📺 观看中".to_string(), Color32::WHITE, th::ACCENT)
                                            } else {
                                                (format!("📺 观看 {desc}"), th::ACCENT_HOVER, th::ACCENT_SOFT)
                                            };
                                            let b = egui::Button::new(RichText::new(txt).size(12.0).color(fg))
                                                .fill(bg)
                                                .corner_radius(CornerRadius::same(10))
                                                .min_size(Vec2::new(0.0, 22.0));
                                            if ui.add(b).on_hover_text(if watching { "关闭画面" } else { "观看屏幕共享" }).clicked() {
                                                watch_toggle = Some((u.id, !watching));
                                            }
                                        } else {
                                            th::pill(ui, "🖥 共享中", th::TEXT_DIM, th::BG_ELEVATED);
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
                                            ui.label(RichText::new("语音线路（两个方向都按这个）").size(12.0).color(th::TEXT_DIM));
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
                                                    egui::ComboBox::from_id_salt(("via", u.id)).selected_text(RichText::new(label).color(if via_now { th::ACCENT_HOVER } else { th::TEXT_DIM })).show_ui(ui, |ui| {
                                                        for n in others {
                                                            ui.selectable_value(&mut r.voice, V::Via(n.clone()), format!("经 {n} 中转"));
                                                        }
                                                    })
                                                    .response
                                                    .on_hover_text("你说给他、他说给你都经这个队友转（他要开着「帮队友中转」、和你们俩都打通）。\n安排不了时走服务器，不会换别人");
                                                });
                                            }
                                            ui.label(RichText::new("画面线路（共享 / 观看）").size(12.0).color(th::TEXT_DIM));
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
                                        let mut v = m.volume(u.id) * 100.0;
                                        if ui.add(egui::Slider::new(&mut v, 0.0..=200.0).suffix("%").text("音量")).changed() {
                                            m.set_volume(u.id, v / 100.0);
                                        }
                                    }
                                    if u.sharing.is_some() && u.share_audio {
                                        let sid = crate::audio::stream_mixer_id(voc_proto::udp::SHARE_AUDIO, u.id);
                                        let mut m = self.mixer.lock().unwrap();
                                        let mut v = m.volume(sid) * 100.0;
                                        if ui
                                            .add(egui::Slider::new(&mut v, 0.0..=200.0).suffix("%").fixed_decimals(0).text("共享声音音量"))
                                            .on_hover_text("看他的屏幕共享时听到的游戏、视频声音，和他说话的音量分开调")
                                            .changed()
                                        {
                                            m.set_volume(sid, v / 100.0);
                                        }
                                    }
                                    if u.music.is_some() {
                                        let mid = crate::audio::stream_mixer_id(voc_proto::udp::MUSIC, u.id);
                                        let mut m = self.mixer.lock().unwrap();
                                        let mut v = m.volume(mid) * 100.0;
                                        if ui.add(egui::Slider::new(&mut v, 0.0..=200.0).suffix("%").text("伴奏音量")).changed() {
                                            m.set_volume(mid, v / 100.0);
                                        }
                                    }
                                    // 嘴到耳延迟：按实际走的线路估算
                                    let one_way = match (via, direct.get(&u.id)) {
                                        (Some(r), _) => r.ms / 2,
                                        (None, Some(q)) if alloc_voice.contains(&u.id) && !q.unstable => q.rtt_ms / 2,
                                        _ => self.voice.ping_ms.load(Ordering::Relaxed).max(1),
                                    };
                                    let (m2e, parts) = latency.mouth_to_ear(u.id, one_way);
                                    ui.label(RichText::new(format!("听他说话的延迟：约 {m2e} ms")).size(12.5).color(th::TEXT));
                                    ui.label(RichText::new(parts).size(11.0).color(th::TEXT_FAINT));
                                    if let Some(r) = via {
                                        ui.label(
                                            RichText::new(th::relay_compare(&name_of(r.via), r.ms, r.server_ms, direct.get(&u.id)))
                                                .size(12.0)
                                                .color(th::TEXT_DIM),
                                        );
                                    } else if let Some(q) = direct.get(&u.id) {
                                        ui.label(
                                            RichText::new(format!(
                                                "P2P：延迟 {} ms · 丢包 {}% · 抖动 {} ms",
                                                q.rtt_ms, q.loss_pct, q.jitter_ms
                                            ))
                                            .size(12.0)
                                            .color(th::TEXT_DIM),
                                        );
                                    }
                                    if let Some((trust, code, _)) = info {
                                        ui.separator();
                                        ui.label(RichText::new("对方的安全码").size(12.0).color(th::TEXT_DIM));
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
        egui::CentralPanel::default().frame(egui::Frame::new().fill(th::BG_PANEL).inner_margin(Margin::symmetric(18, 12))).show(
            ui,
            |ui| {
                if !s.dms.is_empty() {
                    egui::Panel::top("chat_tabs").frame(egui::Frame::new().inner_margin(Margin::symmetric(0, 4))).show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            if ui.selectable_label(s.chat_tab.is_none(), "# 频道聊天").clicked() {
                                switch_tab = Some(None);
                            }
                            for t in &s.dms {
                                let sel = s.chat_tab.as_deref() == Some(t.peer.as_str());
                                let label = if t.unread > 0 { format!("💬 {} ({})", t.peer, t.unread) } else { format!("💬 {}", t.peer) };
                                let mut text = RichText::new(label);
                                if t.unread > 0 {
                                    text = text.color(th::ACCENT_HOVER).strong();
                                }
                                if ui.selectable_label(sel, text).clicked() {
                                    switch_tab = Some(Some(t.peer.clone()));
                                }
                                if sel && ui.small_button("✕").on_hover_text("关闭并清除这段私聊（阅后即焚）").clicked() {
                                    close_dm = Some(t.peer.clone());
                                }
                            }
                        });
                    });
                }
                egui::Panel::bottom("chat_input").frame(egui::Frame::new().inner_margin(Margin::symmetric(0, 8))).show(ui, |ui| {
                    if mic_silent {
                        ui.label(
                            RichText::new("⚠ 你的麦克风一直完全没有声音，别人听不到你。请检查 Windows 设置 → 隐私和安全性 → 麦克风（「允许桌面应用访问麦克风」要打开），或在 设置 → 音频 换一个麦克风。")
                                .size(12.0)
                                .color(th::YELLOW),
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
                                ui.label(RichText::new("提及：").size(12.0).color(th::TEXT_DIM));
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
                        .fill(th::BG_ELEVATED)
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
                egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
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
                                    ui.label(RichText::new(format!("和 {p} 的私聊")).color(th::TEXT_DIM));
                                    ui.label(
                                        RichText::new("只有你们两个看得到 · 端到端加密 · 关闭标签或关掉 VOC 后清除").size(12.0).color(th::TEXT_FAINT),
                                    );
                                }
                                None => {
                                    ui.label(RichText::new("还没有消息").color(th::TEXT_DIM));
                                    ui.label(RichText::new("聊天内容端到端加密，服务器看不到").size(12.0).color(th::TEXT_FAINT));
                                }
                            }
                        });
                    }
                    let mut prev: Option<(&str, u64)> = None;
                    for line in lines {
                        match line {
                            ChatLine::User { from, text, ts, mine, id, reactions } => {
                                // 同一人 5 分钟内的连续消息合并显示
                                let grouped = prev.is_some_and(|(p, t)| p == from && ts.saturating_sub(t) < 300_000);
                                let at_me = !*mine && crate::app::mentions(text, &my_name);
                                // 消息正文：@我 的高亮；能回应的右键出表情
                                let body = |ui: &mut egui::Ui, react: &mut Option<((u32, u64), String)>| {
                                    let mut rt = RichText::new(text);
                                    if at_me {
                                        rt = rt.color(th::YELLOW);
                                    }
                                    let r = ui.add(egui::Label::new(rt).wrap().sense(Sense::click()));
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
                                                let c = if *mine { th::ACCENT_HOVER } else { th::name_color(from) };
                                                ui.label(RichText::new(from).strong().color(c));
                                                ui.label(RichText::new(th::fmt_time(*ts)).size(11.5).color(th::TEXT_FAINT));
                                                if at_me {
                                                    th::pill(ui, "@你", th::YELLOW, th::BG_ELEVATED);
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
                                            let c = if img.mine { th::ACCENT_HOVER } else { th::name_color(&img.from) };
                                            ui.label(RichText::new(&img.from).strong().color(c));
                                            ui.label(RichText::new(th::fmt_time(img.ts)).size(11.5).color(th::TEXT_FAINT));
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
                                            Some((format!("⚠ {e}"), th::RED))
                                        } else if let Some((got, total)) = img.receiving {
                                            Some((format!("🖼 接收中 {}%", got * 100 / total.max(1)), th::TEXT_DIM))
                                        } else if let Some((p, total)) = &img.sending {
                                            let sent = p.load(Ordering::Relaxed);
                                            (sent < *total).then(|| (format!("发送中 {}%", sent * 100 / (*total).max(1)), th::TEXT_DIM))
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
                                        .fill(th::BG_ELEVATED)
                                        .corner_radius(CornerRadius::same(10))
                                        .inner_margin(Margin::symmetric(10, 3))
                                        .show(ui, |ui| {
                                            ui.add(egui::Label::new(RichText::new(t).size(12.0).color(th::TEXT_DIM)).wrap());
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
                    self.views.insert(id, Viewer { view, tex: None, seq: 0, size: (0, 0), low, route: 0 });
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
        if disconnect {
            self.go_login(None);
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
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .default_width(480.0)
            .show(ctx, |ui| {
                ui.set_width(460.0);
                th::segmented(
                    ui,
                    &mut self.settings_tab,
                    &[(0u8, "🎙 音频"), (1, "✨ 降噪"), (4, "🖥 视频"), (2, "🌐 网络"), (3, "🔒 加密"), (5, "ℹ 关于")],
                );
                ui.add_space(10.0);
                egui::ScrollArea::vertical().max_height(ctx.content_rect().height() - 180.0).show(ui, |ui| {
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
                        ui.label(RichText::new(format!("输入：{}\n输出：{}", st.input_desc, st.output_desc)).size(11.5).color(th::TEXT_FAINT));
                    }
                    for e in &self.audio_errors {
                        ui.label(RichText::new(e).color(th::RED));
                    }
                    if ui.small_button("🔄 刷新设备").clicked() {
                        self.refresh_devices();
                        restart = true;
                    }

                    ui.add_space(8.0);
                    th::section(ui, "说话方式");
                    let mut ptt = self.cfg.ptt_vk != 0;
                    if th::segmented(ui, &mut ptt, &[(false, "语音激活"), (true, "按键说话")]) {
                        if ptt {
                            self.ptt_capture = true;
                        } else {
                            self.cfg.ptt_vk = 0;
                            self.ptt_capture = false;
                            self.settings.ptt_vk.store(0, Ordering::Relaxed);
                        }
                    }
                    if self.ptt_capture {
                        ui.label(RichText::new("请按下要用来说话的键（键盘键、鼠标中键或侧键），Esc 取消").color(th::YELLOW));
                        ctx.request_repaint();
                        match crate::app::capture_ptt_key() {
                            Some(Some(vk)) => {
                                self.cfg.ptt_vk = vk;
                                self.settings.ptt_vk.store(vk, Ordering::Relaxed);
                                self.ptt_capture = false;
                            }
                            Some(None) => self.ptt_capture = false,
                            None => {}
                        }
                    } else if self.cfg.ptt_vk != 0 {
                        ui.horizontal(|ui| {
                            ui.label(format!("按住「{}」说话", crate::app::vk_name(self.cfg.ptt_vk)));
                            if ui.button("更改按键").clicked() {
                                self.ptt_capture = true;
                            }
                        });
                        ui.label(RichText::new("在游戏里也有效。松开后多发 0.2 秒，句尾不会被截断。").size(11.5).color(th::TEXT_FAINT));
                    }
                    ui.add_space(8.0);
                    th::section(ui, "快捷键");
                    let mut changed = false;
                    for (i, what) in ["闭麦", "静音"].into_iter().enumerate() {
                        ui.horizontal(|ui| {
                            ui.label(format!("{what}："));
                            if self.hotkey_capture == Some(i) {
                                ui.label(RichText::new("请按下快捷键（可以带 Ctrl / Shift / Alt），Esc 取消").color(th::YELLOW));
                                ctx.request_repaint();
                                if let Some(r) = crate::hotkey::capture() {
                                    if let Some(k) = r {
                                        *if i == 0 { &mut self.cfg.mute_key } else { &mut self.cfg.deafen_key } = k;
                                        changed = true;
                                    }
                                    self.hotkey_capture = None;
                                    self.hotkeys.paused.store(false, Ordering::Relaxed);
                                }
                            } else {
                                let k = if i == 0 { self.cfg.mute_key } else { self.cfg.deafen_key };
                                ui.label(RichText::new(k.label()).strong());
                                if ui.small_button("更改").clicked() {
                                    self.hotkey_capture = Some(i);
                                    self.hotkeys.paused.store(true, Ordering::Relaxed);
                                }
                                if k.is_set() && ui.small_button("清除").clicked() {
                                    *if i == 0 { &mut self.cfg.mute_key } else { &mut self.cfg.deafen_key } = Default::default();
                                    changed = true;
                                }
                            }
                        });
                    }
                    if changed {
                        self.apply_hotkeys();
                    }
                    let clash = |k: crate::hotkey::Hotkey| k.is_set() && !k.ctrl && !k.shift && !k.alt && k.vk == self.cfg.ptt_vk;
                    if self.cfg.mute_key.is_set() && self.cfg.mute_key == self.cfg.deafen_key {
                        ui.label(RichText::new("闭麦和静音用了同一个快捷键，请换一个").size(11.5).color(th::YELLOW));
                    } else if clash(self.cfg.mute_key) || clash(self.cfg.deafen_key) {
                        ui.label(RichText::new("快捷键和按键说话的键一样，请换一个").size(11.5).color(th::YELLOW));
                    } else {
                        ui.label(RichText::new("在游戏里也有效；按下时响一声（低音 = 闭麦 / 静音，高音 = 恢复）").size(11.5).color(th::TEXT_FAINT));
                    }
                    let level = self.settings.input_level_db.get();
                    level_meter(ui, level, self.cfg.vad_threshold_db, self.settings.transmitting.load(Ordering::Relaxed));
                    if self.cfg.ptt_vk == 0 {
                        if ui
                            .add(egui::Slider::new(&mut self.cfg.vad_threshold_db, -80.0..=-10.0).suffix(" dB").text("灵敏度阈值"))
                            .changed()
                        {
                            self.settings.vad_threshold_db.set(self.cfg.vad_threshold_db);
                        }
                        ui.label(RichText::new("说话时电平条要超过白线、不说话时低于白线；背景噪音大就把阈值调高。").size(11.5).color(th::TEXT_FAINT));
                    }
                    if ui
                        .checkbox(&mut self.cfg.aec, "回声消除（不戴耳机、用扬声器外放时开）")
                        .on_hover_text("外放时，扬声器里队友的声音会被麦克风收进去，队友会听到自己的回声。开着它可以消掉。戴耳机时开着也没关系。")
                        .changed()
                    {
                        self.settings.aec.store(self.cfg.aec, Ordering::Relaxed);
                    }
                    let mut gain = self.cfg.input_gain * 100.0;
                    if ui.add(egui::Slider::new(&mut gain, 0.0..=400.0).suffix("%").text("麦克风增益")).changed() {
                        self.cfg.input_gain = gain / 100.0;
                        self.settings.input_gain.set(self.cfg.input_gain);
                    }
                    let mut vol = self.cfg.output_volume * 100.0;
                    if ui.add(egui::Slider::new(&mut vol, 0.0..=200.0).suffix("%").text("输出音量")).changed() {
                        self.cfg.output_volume = vol / 100.0;
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
                    if self.settings_tab == 1 {
                    th::section(ui, "AI 降噪");
                    apply_dn |= th::segmented(
                        ui,
                        &mut self.cfg.denoise,
                        &[(DenoiseMode::Off, "关闭"), (DenoiseMode::Rnnoise, "RNNoise"), (DenoiseMode::Plugin, "Krisp / 插件")],
                    );
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
                    match &self.denoise_status {
                        Ok(st) => ui.label(RichText::new(st).size(12.0).color(th::GREEN)),
                        Err(e) => ui.label(RichText::new(e).size(12.0).color(th::RED)),
                    };

                    ui.label(
                        RichText::new("RNNoise 免费内置，对风扇、空调、键盘声效果好；Krisp 需要向 Krisp 申请 SDK 授权后编译插件。")
                            .size(11.5)
                            .color(th::TEXT_FAINT),
                    );
                    }
                    if self.settings_tab == 2 {
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
                        ui.label(RichText::new(format!("本次流量（从登录起）：{}", crate::traffic::summary())).size(12.0).color(th::TEXT_DIM))
                            .on_hover_text("按量计费的校园网、手机流量可以参考。按收发的包计（含包头估算），不额外发包；退出重新登录后从 0 算");
                        ui.horizontal(|ui| {
                            let n = crate::pathcache::count();
                            ui.label(RichText::new(format!("记住的直连地址：{n} 位队友")).size(12.0).color(th::TEXT_DIM)).on_hover_text(
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
                        ui.label(RichText::new(summary).size(12.0).color(th::TEXT_DIM));
                        ui.label(
                            RichText::new("⚠ 会向同频道成员暴露你的 IP。直连丢包 ≥15%、延迟 >350 ms 或抖动 >80 ms 时自动改走服务器中转。")
                                .size(11.5)
                                .color(th::TEXT_FAINT),
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
                                        Level::Good => ("✔", th::GREEN),
                                        Level::Warn => ("⚠", th::YELLOW),
                                        Level::Bad => ("✖", th::RED),
                                        Level::Pending => ("…", th::TEXT_FAINT),
                                        Level::Info => ("•", th::TEXT_DIM),
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
                                    .color(th::TEXT_DIM),
                            );
                            ui.add_space(4.0);
                            for a in &rep.advice {
                                ui.label(RichText::new(format!("👉 {a}")).size(12.0).color(th::TEXT_DIM));
                            }
                            ui.label(
                                RichText::new("只用连接时已有的测量，不额外发包；这里显示的 IP 只有你自己看得到。")
                                    .size(11.5)
                                    .color(th::TEXT_FAINT),
                            );
                            ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
                        }
                        None => {
                            ui.label(RichText::new("登录服务器后显示").size(12.0).color(th::TEXT_DIM));
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
                    ui.label(RichText::new("开源协议：MIT").size(12.0).color(th::TEXT_DIM));
                    ui.add_space(8.0);
                    th::section(ui, "启动与托盘");
                    ui.checkbox(&mut self.cfg.close_to_tray, "关闭窗口时缩到托盘（语音不断）")
                        .on_hover_text("点右上角 ✕ 后 VOC 留在右下角托盘里继续语音；右键托盘图标可以闭麦、静音或退出。");
                    if self.tray.is_none() && !self.demo.active {
                        ui.label(RichText::new("托盘图标没能创建，关闭窗口会直接退出。").size(11.5).color(th::YELLOW));
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
                            ui.label(RichText::new("已在任务管理器「启动应用」里被禁用，请在那里重新启用。").size(11.5).color(th::YELLOW));
                        }
                        crate::autostart::AutoStart::Unsupported(e) => {
                            ui.label(RichText::new(format!("无法设置开机自启：{e}")).size(11.5).color(th::RED));
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
                            ui.label(RichText::new("已复制").color(th::GREEN));
                        }
                    });
                    ui.checkbox(&mut self.diag_names, "附上编号 ↔ 昵称对照")
                        .on_hover_text("诊断信息和运行日志里队友都写成编号（例如 #12），不写昵称。\n勾上后会在诊断信息末尾附一份「编号是谁」的对照，方便对方看懂；不勾就不带。");
                    ui.label(
                        RichText::new("包含版本、系统、网络质量、声卡和编解码器信息、最近的运行日志；不含聊天、密钥、授权码、服务器地址和任何人的 IP，队友只写编号。")
                            .size(11.5)
                            .color(th::TEXT_FAINT),
                    );
                    match crate::applog::path() {
                        Some(p) => {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(RichText::new("运行日志：").size(12.0).color(th::TEXT_DIM));
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
                                    ui.label(RichText::new("已复制").color(th::GREEN));
                                }
                            });
                        }
                        None => {
                            ui.label(RichText::new("运行日志没有开（设置了 VOC_NO_LOG，或者没有可写的位置）").size(12.0).color(th::TEXT_DIM));
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
                                ui.label(RichText::new("已是最新版本").color(th::GREEN));
                            }
                            crate::update::UpdateState::Available { version, url, .. } => {
                                if self.updater.can_auto_install() {
                                    if ui.button(RichText::new(format!("⬆ 一键更新到 v{version}")).color(th::ACCENT_HOVER)).clicked() {
                                        self.updater.install(ctx.clone());
                                    }
                                } else {
                                    ui.hyperlink_to(RichText::new(format!("发现新版本 v{version}，点此下载")).color(th::ACCENT_HOVER), url);
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
                                ui.label(RichText::new(e).size(12.0).color(th::RED));
                                ui.label(RichText::new("GitHub 和国内镜像都没连上，可以稍后再试").size(11.5).color(th::TEXT_DIM));
                            }
                        }
                    });
                    ui.label(
                        RichText::new("检查更新只访问 GitHub 公开的版本信息，不上传任何本机数据。安装版可以一键更新（自动下载、校验 SHA-256、安装后重新打开）；绿色版请下载新的 zip。设置和身份密钥都会保留。")
                            .size(11.5)
                            .color(th::TEXT_FAINT),
                    );
                    }
                    if self.settings_tab == 4 {
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
                            .color(th::TEXT_FAINT),
                    );
                    ui.add_space(8.0);
                    th::section(ui, "观看时的解码方式");
                    if th::segmented(ui, &mut self.cfg.hw_decode, &[(false, "软解（CPU）"), (true, "硬解（显卡）")]) {
                        video::HW_DECODE.store(self.cfg.hw_decode, Ordering::Relaxed);
                    }
                    let can: Vec<&str> = video::local_decoders().iter().map(|c| c.label()).collect();
                    ui.label(RichText::new(format!("本机可解码：{}", can.join(" · "))).size(12.0).color(th::TEXT_DIM));
                    ui.label(
                        RichText::new("对 H.264 / HEVC 生效：软解延迟更低、兼容性最好；硬解省 CPU（适合同时开游戏）。AV1 总是用显卡硬解（显卡不支持时自动改用 HEVC / H.264）。HEVC / AV1 需要系统装有「HEVC 视频扩展」「AV1 视频扩展」。")
                            .size(11.5)
                            .color(th::TEXT_FAINT),
                    );
                    }
                    if self.settings_tab == 3 {
                    th::section(ui, "端到端加密");
                    ui.label(RichText::new("我的安全码").size(12.0).color(th::TEXT_DIM));
                    let code = voc_proto::e2e::safety_code(&Identity::from_seed(&self.identity_seed).public);
                    egui::Frame::new().fill(th::BG_BASE).corner_radius(CornerRadius::same(8)).inner_margin(Margin::symmetric(10, 6)).show(
                        ui,
                        |ui| {
                            ui.label(RichText::new(code).monospace().size(15.0));
                        },
                    );
                    ui.label(
                        RichText::new("别人核对你的身份时，把这串数字念给对方。它来自你本机的身份密钥，换电脑或重装后会变。")
                            .size(11.5)
                            .color(th::TEXT_FAINT),
                    );
                    }
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
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_width(400.0);
                if let Some(sh) = &self.sharer {
                    let st = &sh.stats;
                    th::pill(ui, format!("● 正在共享 {}", sh.cfg.desc()), th::GREEN, th::GREEN_SOFT);
                    ui.add_space(6.0);
                    let watchers = self.voice.current_link().map(|l| l.watcher_count()).unwrap_or(0);
                    if watchers == 0 {
                        ui.label(RichText::new("还没有人在看（没人看时不编码，不占流量）").color(th::TEXT_DIM));
                    } else {
                        egui::Grid::new("sharestats").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
                            ui.label(RichText::new("观看").color(th::TEXT_DIM));
                            ui.label(format!("{watchers} 人"));
                            ui.end_row();
                            ui.label(RichText::new("分辨率").color(th::TEXT_DIM));
                            ui.label(format!("{}×{}", st.width.load(Ordering::Relaxed), st.height.load(Ordering::Relaxed)));
                            ui.end_row();
                            ui.label(RichText::new("帧率").color(th::TEXT_DIM));
                            ui.label(format!("{} fps（采集 {}）", st.fps.load(Ordering::Relaxed), st.capture_fps.load(Ordering::Relaxed)))
                                .on_hover_text(format!(
                                    "系统这一秒给了 {} 帧画面。系统只在屏幕内容变化时给帧：比所选帧率低，说明屏幕 / 游戏本身就没那么多帧，不是 VOC 丢的",
                                    st.source_fps.load(Ordering::Relaxed)
                                ));
                            ui.end_row();
                            ui.label(RichText::new("码率").color(th::TEXT_DIM));
                            let mbps = st.kbps.load(Ordering::Relaxed) as f32 / 1000.0;
                            let skipped = st.static_skipped.load(Ordering::Relaxed);
                            let auto_h = st.auto_height.load(Ordering::Relaxed);
                            let fec = match st.fec_pct.load(Ordering::Relaxed) {
                                0 => String::new(),
                                p => format!(" · 有丢包，已加 {p}% 冗余"),
                            };
                            let pct = st.rate_pct.load(Ordering::Relaxed);
                            if auto_h > 0 {
                                ui.label(RichText::new(format!("{mbps:.1} Mbps · 网络拥堵，已自动降到 {auto_h}P{fec}")).color(th::YELLOW));
                            } else if pct < 100 && st.low_watchers.load(Ordering::Relaxed) > 0 {
                                ui.label(RichText::new(format!("{mbps:.1} Mbps · 流畅层已自动降到 {pct}%{fec}")).color(th::YELLOW));
                            } else if fec.is_empty() {
                                ui.label(format!("{mbps:.1} Mbps"));
                            } else {
                                ui.label(RichText::new(format!("{mbps:.1} Mbps{fec}")).color(th::YELLOW));
                            }
                            ui.end_row();
                            if skipped > 0 {
                                ui.label(RichText::new("省流量").color(th::TEXT_DIM));
                                ui.label(format!("画面变化小，每秒少发 {skipped} 帧"));
                                ui.end_row();
                            }
                            ui.label(RichText::new("编码").color(th::TEXT_DIM));
                            ui.label(format!("{} · {} ms/帧", st.encoder.lock().unwrap(), st.encode_ms.load(Ordering::Relaxed)));
                            ui.end_row();
                            ui.label(RichText::new("画面处理").color(th::TEXT_DIM));
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
                    if let Some(e) = st.error.lock().unwrap().as_ref() {
                        ui.label(RichText::new(e).color(th::RED));
                    }
                    ui.add_space(8.0);
                    let b = egui::Button::new(RichText::new("⏹ 停止共享").strong().color(Color32::WHITE))
                        .fill(th::RED)
                        .corner_radius(CornerRadius::same(10))
                        .min_size(Vec2::new(ui.available_width(), 36.0));
                    if ui.add(b).clicked() {
                        self.sharer = None;
                        self.share_confirmed = false;
                        s.send(ClientMsg::ShareStop);
                    }
                    return;
                }
                th::section(ui, "显示器");
                let cur = self.monitors.get(self.cfg.share.monitor).cloned().unwrap_or_else(|| "主显示器".into());
                egui::ComboBox::from_id_salt("mon").width(ui.available_width()).selected_text(cur).show_ui(ui, |ui| {
                    for (i, m) in self.monitors.iter().enumerate() {
                        ui.selectable_value(&mut self.cfg.share.monitor, i, m);
                    }
                });
                ui.add_space(6.0);
                th::section(ui, "分辨率");
                th::segmented(ui, &mut self.cfg.share.height, &[(480, "480P"), (720, "720P"), (1080, "1080P")]);
                ui.add_space(6.0);
                th::section(ui, "帧率");
                th::segmented(ui, &mut self.cfg.share.fps, &[(30, "30 帧"), (60, "60 帧")]);
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
                    ui.label(RichText::new(format!("低带宽模式：实际按 {} 共享", eff.desc())).size(12.0).color(th::YELLOW));
                }
                ui.label(
                    RichText::new(format!(
                        "每位观看者约 {:.1} Mbps（H.264；AV1 约 {:.1}）· 端到端加密 · 能直连的不经过服务器",
                        eff.bitrate() as f32 / 1e6,
                        eff.bitrate_for(crate::mf::enc::Codec::Av1) as f32 / 1e6
                    ))
                    .size(12.0)
                    .color(th::TEXT_DIM),
                );
                if let Some(e) = &self.share_error {
                    ui.label(RichText::new(e).color(th::RED));
                }
                ui.add_space(6.0);
                let can_audio = crate::loopback::supported();
                ui.add_enabled(can_audio, egui::Checkbox::new(&mut self.cfg.share_audio, "同时共享电脑声音（不含 VOC 自己的声音）"))
                    .on_hover_text("游戏、视频的声音会一起发给正在看的人；队友的说话声、提示音不会被录进去。")
                    .on_disabled_hover_text("共享电脑声音需要 Windows 11");
                ui.checkbox(&mut self.cfg.share.delay_bwe, "按延迟变化提前降码率").on_hover_text(
                    "画面走的线路往返时间开始变大（排队了、还没丢包）就先降码率，线路空了再升回来，减少卡顿。\n\
                     打开后「原画」的码率也会跟着调（默认原画固定码率、保证清楚）。",
                );
                ui.checkbox(&mut self.cfg.share.p2p_only, "只通过 P2P 直连发送（不经服务器）").on_hover_text(
                    "画面、补发、鼠标、共享声音都不发给服务器，服务器带宽小时有用。\n\
                     能打通直连的人全部直连发，不受「直连上行预算」限制：看的人多时可能占满你的上行。\n\
                     和你打不通、也没有别的观看者能转给他的人看不了（他那边会看到原因）。",
                );
                ui.add_space(8.0);
                start = th::primary_button(ui, "开始共享", ui.available_width(), true).clicked();
            });
        if start {
            let eff = self.cfg.share.effective(self.cfg.low_bandwidth);
            match Sharer::start(eff.clone(), self.voice.clone()) {
                Ok(sh) => {
                    s.send(ClientMsg::ShareStart {
                        desc: eff.desc(),
                        audio: self.cfg.share_audio && crate::loopback::supported(),
                        simulcast: true,
                        rtx: true,
                        bps: eff.bitrate(),
                        p2p_only: eff.p2p_only,
                    });
                    self.sharer = Some(sh);
                    self.share_confirmed = false;
                    self.share_error = None;
                    self.show_share = false;
                }
                Err(e) => {
                    crate::applog::log(format!("屏幕共享开不了：{e}"));
                    self.share_error = Some(e);
                }
            }
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
                ui.label(RichText::new(format!("{w} × {h} · 阅后即焚：关闭 VOC 后图片会被清除，无法保存")).size(11.5).color(th::TEXT_FAINT));
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
        let voice = self.voice.clone();
        let mut go_full: Option<u32> = None;
        let mut exit_full = false;
        let mixer = self.mixer.clone();
        if ctx.input(|i| i.pointer.delta() != Vec2::ZERO) {
            self.fs_mouse_at = std::time::Instant::now();
        }
        for (id, v) in self.views.iter_mut() {
            if let Some(f) = v.view.frame.lock().unwrap().as_ref() {
                if f.seq != v.seq {
                    let img = egui::ColorImage::from_rgba_unmultiplied([f.width, f.height], &f.rgba);
                    match &mut v.tex {
                        Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                        None => v.tex = Some(ctx.load_texture(format!("screen-{id}"), img, egui::TextureOptions::LINEAR)),
                    }
                    v.seq = f.seq;
                    v.size = (f.width, f.height);
                }
            }
            let user = s.users.iter().find(|u| u.id == *id);
            let name = user.map(|u| u.name.clone()).unwrap_or_default();
            let desc = user.and_then(|u| u.sharing.clone()).unwrap_or_default();
            // 全屏：整个窗口只显示画面，鼠标 2 秒不动就隐藏工具栏
            if self.fullscreen == Some(*id) {
                let screen = ctx.content_rect();
                let show_bar = self.fs_mouse_at.elapsed() < std::time::Duration::from_secs(2);
                egui::Area::new(egui::Id::unique(("fullscreen", *id))).fixed_pos(screen.min).order(egui::Order::Foreground).show(ctx, |ui| {
                    ui.painter().rect_filled(screen, 0.0, Color32::BLACK);
                    let (rect, resp) = ui.allocate_exact_size(screen.size(), Sense::click());
                    if let Some(t) = &v.tex {
                        let (w, h) = (v.size.0.max(1) as f32, v.size.1.max(1) as f32);
                        let scale = (rect.width() / w).min(rect.height() / h);
                        let img = egui::Rect::from_center_size(rect.center(), Vec2::new(w * scale, h * scale));
                        ui.painter().image(t.id(), img, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
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
                            ui.label(RichText::new(format!("📺 {name} 的屏幕 · {desc} · {} fps", v.view.fps.load(Ordering::Relaxed))).color(Color32::WHITE));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui.button("退出全屏").clicked() {
                                    exit_full = true;
                                }
                                if user.is_some_and(|u| u.share_audio) {
                                    let sid = crate::audio::stream_mixer_id(voc_proto::udp::SHARE_AUDIO, *id);
                                    let mut m = mixer.lock().unwrap();
                                    let mut vol = m.volume(sid) * 100.0;
                                    if ui.add(egui::Slider::new(&mut vol, 0.0..=200.0).suffix("%").text("🔊")).changed() {
                                        m.set_volume(sid, vol / 100.0);
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
            let mut open = true;
            egui::Window::new(RichText::new(format!("📺 {name} 的屏幕")).strong())
                .id(egui::Id::unique(("viewer", *id)))
                .open(&mut open)
                .default_size([960.0, 580.0])
                .resizable(true)
                .frame(egui::Frame::window(&ctx.global_style()).fill(Color32::BLACK).inner_margin(Margin::same(8)))
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        th::pill(ui, desc.clone(), th::ACCENT_HOVER, th::ACCENT_SOFT);
                        th::pill(ui, format!("{}×{}", v.size.0, v.size.1), th::TEXT_DIM, th::BG_ELEVATED);
                        th::pill(ui, format!("{} fps", v.view.fps.load(Ordering::Relaxed)), th::TEXT_DIM, th::BG_ELEVATED);
                        let dec = v.view.decoder.lock().unwrap().clone();
                        if !dec.is_empty() {
                            th::pill(ui, dec, th::TEXT_DIM, th::BG_ELEVATED);
                        }
                        th::pill(ui, "🔒 端到端加密", th::GREEN, th::GREEN_SOFT);
                        if user.is_some_and(|u| u.simulcast) {
                            let before = v.low;
                            ui.selectable_value(&mut v.low, false, "原画").on_hover_text(format!("按共享者设置的 {desc} 观看"));
                            ui.selectable_value(&mut v.low, true, "流畅").on_hover_text("360P 低画质，省流量、网络差时更不容易卡");
                            if v.low != before {
                                relayer.push((*id, v.low, v.route));
                            }
                        }
                        {
                            let before = v.route;
                            ui.separator();
                            ui.selectable_value(&mut v.route, 0, "自动").on_hover_text("能直连就直连，否则经服务器（推荐）");
                            ui.selectable_value(&mut v.route, 1, "P2P").on_hover_text(
                                "画面直接从共享者发给你，不受对方上行预算限制；和对方打不通时仍经服务器。\n双方都要 0.4.2 以上，服务器也要升级",
                            );
                            ui.selectable_value(&mut v.route, 2, "服务器")
                                .on_hover_text("画面经服务器转给你。直连卡顿、花屏或一直等待画面时试试\n双方都要 0.4.2 以上，服务器也要升级");
                            if v.route != before {
                                relayer.push((*id, v.low, v.route));
                            }
                            // 播放方式：流畅优先（匀速，默认）/ 延迟优先（到了就显示）
                            ui.separator();
                            let mut smooth = v.view.smooth.load(Ordering::Relaxed);
                            let was = smooth;
                            ui.selectable_value(&mut smooth, true, "流畅优先")
                                .on_hover_text("画面成串到达时按原来的间隔匀速显示，看着更顺；网络稳时只多一两帧的延迟");
                            ui.selectable_value(&mut smooth, false, "延迟优先").on_hover_text("画面一到就显示，延迟最低；网络抖时会一顿一顿的");
                            if smooth != was {
                                v.view.smooth.store(smooth, Ordering::Relaxed);
                                smooth_change = Some(smooth);
                            }
                            let p2p_only = user.is_some_and(|u| u.share_p2p_only);
                            match voice.video_direct(*id) {
                                Some(true) => th::pill(ui, "⇄ 直连中", th::GREEN, th::GREEN_SOFT),
                                // 经别的观看者转来的（只走 P2P 时不会经服务器）
                                Some(false) if p2p_only => th::pill(ui, "经队友转发", th::TEXT_DIM, th::BG_ELEVATED),
                                Some(false) => th::pill(ui, "经服务器", th::TEXT_DIM, th::BG_ELEVATED),
                                None => th::pill(ui, "没收到画面", th::YELLOW, th::BG_ELEVATED),
                            }
                            .on_hover_text("现在画面实际走的线路（最近 2 秒）");
                            if p2p_only {
                                th::pill(ui, "共享者只走 P2P", th::TEXT_DIM, th::BG_ELEVATED)
                                    .on_hover_text("共享者设置了画面不经服务器：和他打不通直连、也没有别的观看者能转给你时看不了");
                            }
                        }
                        // 选项冲突时以更保守的一方为准，并说明原因（第 46 / 44 项）
                        if user.is_some_and(|u| u.share_p2p_only) && voice.video_direct(*id).is_none() {
                            let why = if v.route == 2 {
                                "共享者设置了「只走 P2P」，而你选了画面「服务器」：两边都按更保守的来，这样看不了。改选「自动」或「P2P」试试。"
                            } else {
                                "共享者设置了「只走 P2P」（画面不经服务器），你和他之间还没打通直连，也没有别的观看者能转给你。打洞成功后会自动出画面。"
                            };
                            ui.label(RichText::new(why).size(12.0).color(th::YELLOW));
                        }
                        // 共享声音的音量：只影响你自己听到的大小，和他说话的音量分开调
                        {
                            ui.separator();
                            let has_audio = user.is_some_and(|u| u.share_audio);
                            let sid = crate::audio::stream_mixer_id(voc_proto::udp::SHARE_AUDIO, *id);
                            let mut m = mixer.lock().unwrap();
                            let mut vol = m.volume(sid) * 100.0;
                            ui.add_enabled_ui(has_audio, |ui| {
                                let icon = if vol < 0.5 { "🔇" } else { "🔊" };
                                if ui
                                    .button(icon)
                                    .on_hover_text("静音 / 恢复共享声音")
                                    .on_disabled_hover_text("对方没有勾选「同时共享电脑声音」")
                                    .clicked()
                                {
                                    vol = if vol < 0.5 { 100.0 } else { 0.0 };
                                    m.set_volume(sid, vol / 100.0);
                                }
                                ui.label(RichText::new("共享声音").size(12.0).color(th::TEXT_DIM));
                                if ui
                                    .add(egui::Slider::new(&mut vol, 0.0..=200.0).suffix("%").fixed_decimals(0))
                                    .on_hover_text("只调你听到的共享声音大小（游戏、视频的声音），不影响他说话的音量；右键他的名字也能调")
                                    .on_disabled_hover_text("对方没有勾选「同时共享电脑声音」")
                                    .changed()
                                {
                                    m.set_volume(sid, vol / 100.0);
                                }
                            });
                        }
                        if ui.button("全屏").on_hover_text("全屏观看（双击画面也可以；Esc 退出）").clicked() {
                            go_full = Some(*id);
                        }
                    });
                    match &v.tex {
                        Some(t) => {
                            let avail = ui.available_size();
                            let (w, h) = (v.size.0.max(1) as f32, v.size.1.max(1) as f32);
                            let scale = (avail.x / w).min(avail.y / h).max(0.05);
                            ui.centered_and_justified(|ui| {
                                let r = ui.add(
                                    egui::Image::new(egui::load::SizedTexture::new(t.id(), Vec2::new(w * scale, h * scale))).sense(Sense::click()),
                                );
                                draw_remote_cursor(ui.painter(), r.rect, &v.view);
                                if r.double_clicked() {
                                    go_full = Some(*id);
                                }
                            });
                        }
                        None => {
                            ui.centered_and_justified(|ui| ui.label(RichText::new("正在等待画面…").color(th::TEXT_DIM)));
                        }
                    }
                });
            if !open {
                close.push(*id);
            }
        }
        if let Some(sm) = smooth_change {
            self.cfg.smooth_playback = sm;
            video::SMOOTH_PLAYBACK.store(sm, Ordering::Relaxed);
        }
        for (id, low, route) in relayer {
            s.send(ClientMsg::Watch { target: id, on: true, low, route, cursor: true });
        }
        for id in close {
            self.views.remove(&id);
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
                    ui.label(RichText::new("伴奏需要 Windows 11（Windows 10 build 20348 以上）。").color(th::YELLOW));
                    return;
                }
                if !in_channel {
                    ui.label(RichText::new("进入语音频道后才能放伴奏。").color(th::TEXT_DIM));
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
                            .color(th::TEXT_FAINT),
                    );
                    ui.add_space(8.0);
                    if ui.button("⏹ 停止伴奏").clicked() {
                        stop = true;
                    }
                } else {
                    ui.label(RichText::new("选一个正在出声的程序（音乐播放器等），把它的声音放给频道里的人。不经过麦克风，闭麦也照放。").size(12.0).color(th::TEXT_DIM));
                    ui.add_space(6.0);
                    if ui.button("🔄 刷新程序列表").clicked() {
                        refresh = true;
                    }
                    ui.add_space(4.0);
                    if self.music_apps.is_empty() {
                        ui.label(RichText::new("没有找到出声的程序：先让程序播放一下，再点刷新。").size(12.0).color(th::TEXT_FAINT));
                    }
                    for (pid, name) in &self.music_apps {
                        if ui.add(egui::Button::new(format!("▶ {name}")).min_size(Vec2::new(ui.available_width(), 26.0))).clicked() {
                            start = Some((*pid, name.clone()));
                        }
                    }
                }
                if let Some(e) = &self.music_error {
                    ui.label(RichText::new(e).color(th::RED));
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
        th::BG_ACTIVE
    } else if resp.hovered() && !full {
        th::BG_HOVER
    } else {
        Color32::TRANSPARENT
    };
    p.rect_filled(rect, CornerRadius::same(8), bg);
    if here {
        let bar = egui::Rect::from_min_size(rect.min + Vec2::new(0.0, 7.0), Vec2::new(3.0, rect.height() - 14.0));
        p.rect_filled(bar, CornerRadius::same(2), th::ACCENT);
    }
    let fg = if here {
        Color32::WHITE
    } else if full {
        th::TEXT_FAINT
    } else {
        th::TEXT_DIM
    };
    p.text(rect.left_center() + Vec2::new(12.0, 0.0), Align2::LEFT_CENTER, format!("🔊  {name}"), egui::FontId::proportional(14.5), fg);
    if count > 0 {
        let txt = format!("{count}/{}", voc_proto::MAX_CHANNEL_USERS);
        let c = if full { th::RED } else { th::TEXT_FAINT };
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
        let name = RichText::new(&u.name).color(if speaking { Color32::WHITE } else { th::TEXT_DIM });
        ui.label(if speaking { name.strong() } else { name });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            right(ui);
        });
    });
    let resp = row.response.interact(Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(row.response.rect.expand(2.0), CornerRadius::same(8), th::BG_HOVER.gamma_multiply(0.35));
    }
    resp
}

fn device_combo(ui: &mut egui::Ui, id: &str, sel: &mut Option<String>, list: &[String]) -> bool {
    let before = sel.clone();
    egui::ComboBox::from_id_salt(id)
        .width(330.0)
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
    p.rect_filled(rect, CornerRadius::same(6), th::BG_BASE);
    let fill = egui::Rect::from_min_max(rect.min, egui::pos2(to_x(level_db), rect.max.y));
    p.rect_filled(fill, CornerRadius::same(6), if active { th::GREEN } else { th::BG_ACTIVE });
    let x = to_x(thr_db);
    p.line_segment([egui::pos2(x, rect.top() - 3.0), egui::pos2(x, rect.bottom() + 3.0)], Stroke::new(2.0, Color32::WHITE));
}

// ============================================================ eframe 入口

impl eframe::App for App {
    /// 窗口隐藏（缩到托盘）时 eframe 不调用 ui()，只调用 logic()：网络和托盘都放这里
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.background(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // 闭麦 / 静音快捷键由后台线程全局检测（hotkey.rs），窗口在不在前台都一样
        self.handle_hotkeys();

        match &self.screen {
            Screen::Login { error } => {
                let e = error.clone();
                self.ui_login(ui, e, false);
            }
            Screen::Connecting(_) => self.ui_login(ui, None, true),
            Screen::Main(_) => self.ui_main(ui),
            Screen::Reconnecting(_) => self.ui_reconnecting(ui),
        }
        let ctx = ui.ctx().clone();
        self.ui_settings(&ctx);
        self.ui_share(&ctx);
        self.ui_viewers(&ctx);
        self.ui_image_view(&ctx);
        self.ui_music(&ctx);
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
