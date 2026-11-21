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
        let Screen::Main(s) = &mut self.screen else { return };
        let me = s.me().cloned();
        let my_channel = me.as_ref().and_then(|u| u.channel_id);
        let mut disconnect = false;
        let mut toggled_state = false;
        let mut trust_id: Option<u32> = None;
        let mut watch_toggle: Option<(u32, bool)> = None;

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
                            let (mi, mt) = if muted { ("🎙", "取消闭麦（Ctrl+M）") } else { ("🎙", "闭麦（Ctrl+M）") };
                            if th::icon_button(ui, mi, mt, if muted { Tone::Danger } else { Tone::Normal }).clicked() {
                                self.settings.muted.store(!muted, Ordering::Relaxed);
                                toggled_state = true;
                            }
                            let (di, dt) = if deaf { ("🔇", "取消静音（Ctrl+D）") } else { ("🎧", "静音：听不到别人（Ctrl+D）") };
                            if th::icon_button(ui, di, dt, if deaf { Tone::Danger } else { Tone::Normal }).clicked() {
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
                            if link_state.is_some() || u.sharing.is_some() || relaying > 0 {
                                ui.horizontal(|ui| {
                                    ui.add_space(48.0);
                                    ui.spacing_mut().item_spacing.x = 4.0;
                                    let voice_direct = alloc_voice.contains(&u.id);
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
                                    {
                                        let mut m = self.mixer.lock().unwrap();
                                        let mut v = m.volume(u.id) * 100.0;
                                        if ui.add(egui::Slider::new(&mut v, 0.0..=200.0).suffix("%").text("音量")).changed() {
                                            m.set_volume(u.id, v / 100.0);
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
                            member_row(ui, u, false, |_| {});
                        }
                    }
                });
            });

        // ---------------- 中间：文字聊天
        egui::CentralPanel::default().frame(egui::Frame::new().fill(th::BG_PANEL).inner_margin(Margin::symmetric(18, 12))).show(
            ui,
            |ui| {
                egui::Panel::bottom("chat_input").frame(egui::Frame::new().inner_margin(Margin::symmetric(0, 8))).show(ui, |ui| {
                    egui::Frame::new()
                        .fill(th::BG_ELEVATED)
                        .corner_radius(CornerRadius::same(12))
                        .inner_margin(Margin::symmetric(12, 6))
                        .show(ui, |ui| {
                            let r = ui.add(
                                egui::TextEdit::singleline(&mut s.chat_input)
                                    .hint_text("发送消息…（回车发送 · 端到端加密）")
                                    .frame(egui::Frame::NONE)
                                    .desired_width(f32::INFINITY)
                                    .margin(Margin::symmetric(0, 6)),
                            );
                            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                let text = std::mem::take(&mut s.chat_input);
                                let text = text.trim().chars().take(voc_proto::MAX_CHAT_LEN).collect::<String>();
                                if !text.is_empty() {
                                    if let Some(e2e) = &s.e2e {
                                        let (_, missing) = e2e.lock().unwrap().send_chat(&text);
                                        if !missing.is_empty() {
                                            s.chat.push(ChatLine::System(format!(
                                                "以下成员暂时收不到这条消息（加密尚未建立或身份待核对）：{}",
                                                missing.join("、")
                                            )));
                                        }
                                    }
                                    let me = s.me().map(|u| u.name.clone()).unwrap_or_default();
                                    s.chat.push(ChatLine::User { from: me, text, ts: voc_proto::now_millis(), mine: true });
                                }
                                r.request_focus();
                            }
                        });
                });
                egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
                    if s.chat.is_empty() {
                        ui.add_space(ui.available_height() * 0.35);
                        ui.vertical_centered(|ui| {
                            ui.label(RichText::new("💬").size(32.0));
                            ui.label(RichText::new("还没有消息").color(th::TEXT_DIM));
                            ui.label(RichText::new("聊天内容端到端加密，服务器看不到").size(12.0).color(th::TEXT_FAINT));
                        });
                    }
                    let mut prev: Option<(&str, u64)> = None;
                    for line in &s.chat {
                        match line {
                            ChatLine::User { from, text, ts, mine } => {
                                // 同一人 5 分钟内的连续消息合并显示
                                let grouped = prev.is_some_and(|(p, t)| p == from && ts.saturating_sub(t) < 300_000);
                                if grouped {
                                    ui.horizontal(|ui| {
                                        ui.add_space(48.0);
                                        ui.add(egui::Label::new(RichText::new(text)).wrap());
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
                                            });
                                            ui.add(egui::Label::new(RichText::new(text)).wrap());
                                        });
                                    });
                                }
                                prev = Some((from.as_str(), *ts));
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
        if let Some((id, on)) = watch_toggle {
            s.send(ClientMsg::Watch { target: id, on });
            if on {
                if let Some(view) = self.voice.video.watch(id, true) {
                    self.views.insert(id, Viewer { view, tex: None, seq: 0, size: (0, 0) });
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
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.cfg.sfx, "进出频道提示音");
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
                        .checkbox(&mut self.cfg.p2p, "P2P 直连（省服务器带宽、延迟更低）")
                        .on_hover_text("能直连的队友之间语音不经过服务器。代价：同频道开启了 P2P 的成员能看到你的 IP 地址。")
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
                    ui.label(
                        RichText::new("包含版本、系统、网络质量、声卡和编解码器信息；不含聊天、密钥、授权码、服务器地址和任何人的 IP。")
                            .size(11.5)
                            .color(th::TEXT_FAINT),
                    );
                    ui.add_space(8.0);
                    th::section(ui, "更新");
                    ui.checkbox(&mut self.cfg.auto_update, "启动时自动检查更新");
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
                            }
                            crate::update::UpdateState::Downloading { percent, .. } => {
                                ui.add(egui::ProgressBar::new(percent as f32 / 100.0).desired_width(160.0).show_percentage());
                            }
                            crate::update::UpdateState::Launched => {
                                ui.label("正在启动安装程序…");
                            }
                            crate::update::UpdateState::Failed(e) => {
                                ui.label(RichText::new(e).size(12.0).color(th::RED));
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
                            ui.label(format!("{} fps（采集 {}）", st.fps.load(Ordering::Relaxed), st.capture_fps.load(Ordering::Relaxed)));
                            ui.end_row();
                            ui.label(RichText::new("码率").color(th::TEXT_DIM));
                            ui.label(format!("{:.1} Mbps", st.kbps.load(Ordering::Relaxed) as f32 / 1000.0));
                            ui.end_row();
                            ui.label(RichText::new("编码").color(th::TEXT_DIM));
                            ui.label(format!("{} · {} ms/帧", st.encoder.lock().unwrap(), st.encode_ms.load(Ordering::Relaxed)));
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
                th::section(ui, "编码器");
                let choices = video::encoder_choices();
                let cur = choices.iter().find(|c| c.0 == self.cfg.share.encoder).map(|c| c.1.clone()).unwrap_or_else(|| choices[0].1.clone());
                egui::ComboBox::from_id_salt("share-encoder").width(ui.available_width()).selected_text(cur).show_ui(ui, |ui| {
                    for (k, label) in &choices {
                        ui.selectable_value(&mut self.cfg.share.encoder, k.clone(), label);
                    }
                });
                ui.add_space(8.0);
                ui.label(
                    RichText::new(format!(
                        "每位观看者约 {:.1} Mbps（H.264；AV1 约 {:.1}）· 端到端加密 · 能直连的不经过服务器",
                        self.cfg.share.bitrate() as f32 / 1e6,
                        self.cfg.share.bitrate_for(crate::mf::enc::Codec::Av1) as f32 / 1e6
                    ))
                    .size(12.0)
                    .color(th::TEXT_DIM),
                );
                if let Some(e) = &self.share_error {
                    ui.label(RichText::new(e).color(th::RED));
                }
                ui.add_space(8.0);
                start = th::primary_button(ui, "开始共享", ui.available_width(), true).clicked();
            });
        if start {
            match Sharer::start(self.cfg.share.clone(), self.voice.clone()) {
                Ok(sh) => {
                    s.send(ClientMsg::ShareStart { desc: self.cfg.share.desc() });
                    self.sharer = Some(sh);
                    self.share_confirmed = false;
                    self.share_error = None;
                    self.show_share = false;
                }
                Err(e) => self.share_error = Some(e),
            }
        }
        if !open {
            self.show_share = false;
        }
    }

    fn ui_viewers(&mut self, ctx: &egui::Context) {
        let Screen::Main(s) = &mut self.screen else { return };
        let mut close = Vec::new();
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
                    });
                    match &v.tex {
                        Some(t) => {
                            let avail = ui.available_size();
                            let (w, h) = (v.size.0.max(1) as f32, v.size.1.max(1) as f32);
                            let scale = (avail.x / w).min(avail.y / h).max(0.05);
                            ui.centered_and_justified(|ui| {
                                ui.add(egui::Image::new(egui::load::SizedTexture::new(t.id(), Vec2::new(w * scale, h * scale))));
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
        for id in close {
            self.views.remove(&id);
            self.voice.video.watch(id, false);
            s.send(ClientMsg::Watch { target: id, on: false });
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
        let (m, d) = ui.input(|i| {
            (i.modifiers.command && i.key_pressed(egui::Key::M), i.modifiers.command && i.key_pressed(egui::Key::D))
        });
        if m {
            self.settings.muted.fetch_xor(true, Ordering::Relaxed);
            self.sync_voice_state();
        }
        if d {
            self.settings.deafened.fetch_xor(true, Ordering::Relaxed);
            self.sync_voice_state();
        }

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
        eframe::set_value(storage, eframe::APP_KEY, &self.cfg);
    }
}
