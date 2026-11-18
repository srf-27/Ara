#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

//! Minimal Windows GUI: room form, member list, connection state and log panel.
//! TUN wiring and transfer progress are wired in later stage; this binary is
//! intentionally console-less (`windows_subsystem = "windows"`).

use std::net::Ipv4Addr;
use std::sync::mpsc::{channel, Receiver};

use ara::client::UiEvent;
use ara::config::ResolvedConfig;
use eframe::egui;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([980.0, 640.0])
            .with_min_inner_size([900.0, 600.0])
            .with_title("Ara"),
        ..Default::default()
    };
    eframe::run_native("Ara", options, Box::new(|_cc| Ok(Box::new(AraApp::new()))))
}

struct AraApp {
    server: String,
    room: String,
    password: String,
    nickname: String,
    create: bool,
    status: String,
    room_code: String,
    vip: Option<Ipv4Addr>,
    members: Vec<(String, Ipv4Addr)>,
    log: Vec<String>,
    rx: Option<Receiver<UiEvent>>,
    connected: bool,
}

impl AraApp {
    fn new() -> Self {
        let mut app = Self {
            server: "127.0.0.1:23947".into(),
            room: String::new(),
            password: String::new(),
            nickname: String::new(),
            create: false,
            status: "未连接".into(),
            room_code: String::new(),
            vip: None,
            members: Vec::new(),
            log: Vec::new(),
            rx: None,
            connected: false,
        };
        if let Some(snapshot) = ara::room_state::load() {
            app.room_code = snapshot.room_code.clone();
            app.vip = snapshot.virtual_ip;
            app.members = snapshot
                .members
                .iter()
                .map(|m| (m.nickname.clone(), m.virtual_ip))
                .collect();
            app.log.push(format!(
                "本地快照：房间 {}，{} 位成员",
                snapshot.room_code,
                snapshot.members.len()
            ));
        }
        app
    }

    fn connect(&mut self) {
        if self.connected {
            return;
        }
        let Ok(server) = self.server.parse::<std::net::SocketAddr>() else {
            self.status = "服务器地址无效".into();
            return;
        };
        if self.room.is_empty() && !self.create {
            self.status = "请填写房间号或选择创建".into();
            return;
        }
        if self.create && self.password.is_empty() {
            self.status = "创建房间需要密码".into();
            return;
        }
        let cfg = ResolvedConfig {
            network: String::new(),
            secret: String::new(),
            server_host: server.ip().to_string(),
            server_port: server.port(),
            local_port: 0,
            tcp_port: 0,
            identity_path: None,
            room_code: self.room.clone(),
            room_password: self.password.clone(),
            access_token: String::new(),
            nickname: self.nickname.clone(),
            create_room: self.create,
            server_pub: String::new(),
            direct_peer_addr: None,
            direct_peer_key: String::new(),
            direct_peer_vip: None,
            direct_vip: None,
        };
        let (tx, rx) = channel::<UiEvent>();
        let (etx, mut erx) = tokio::sync::mpsc::unbounded_channel();
        self.rx = Some(rx);
        self.status = "连接中…".into();
        self.connected = true;
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = tx.send(UiEvent::Status(format!("runtime: {e}")));
                    return;
                }
            };
            let fwd = tx.clone();
            rt.spawn(async move {
                while let Some(ev) = erx.recv().await {
                    if fwd.send(ev).is_err() {
                        break;
                    }
                }
            });
            let (_ip_out_tx, ip_out_rx) = tokio::sync::mpsc::channel(256);
            let (ip_in_tx, mut ip_in_rx) = tokio::sync::mpsc::channel(256);
            rt.spawn(async move {
                while ip_in_rx.recv().await.is_some() {}
            });
            let (assigned_tx, _assigned_rx) = tokio::sync::oneshot::channel();
            let result =
                rt.block_on(ara::client::run(cfg, ip_out_rx, ip_in_tx, assigned_tx, Some(etx)));
            if let Err(e) = result {
                let _ = tx.send(UiEvent::Status(format!("连接结束：{e}")));
            }
        });
    }

    fn poll_events(&mut self) {
        let Some(rx) = &self.rx else { return };
        while let Ok(ev) = rx.try_recv() {
            match ev {
                UiEvent::Welcome { virtual_ip, room_code } => {
                    self.vip = Some(virtual_ip);
                    self.room_code = room_code.clone();
                    self.status = format!("已连接房间 {room_code}，VIP {virtual_ip}");
                    self.log.push(format!("加入房间 {room_code}，分配 {virtual_ip}"));
                }
                UiEvent::Peers(peers) => {
                    self.members = peers
                        .iter()
                        .map(|p| (p.nickname.clone(), p.virtual_ip))
                        .collect();
                    self.log.push(format!("成员更新：{} 人", self.members.len()));
                }
                UiEvent::Kicked(reason) => {
                    self.status = format!("已被服务器移出：{reason}");
                    self.log.push(format!("被移出：{reason}"));
                    self.connected = false;
                }
                UiEvent::Status(s) => {
                    self.log.push(s.clone());
                    self.status = s;
                }
            }
        }
    }
}

impl eframe::App for AraApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_events();
        ctx.request_repaint_after(std::time::Duration::from_millis(250));

        egui::TopBottomPanel::top("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Ara");
                ui.separator();
                let color = if self.connected {
                    egui::Color32::from_rgb(34, 139, 87)
                } else {
                    egui::Color32::from_rgb(140, 140, 140)
                };
                ui.colored_label(color, &self.status);
                if let Some(vip) = self.vip {
                    ui.separator();
                    ui.label(format!("VIP {vip}"));
                }
            });
        });

        egui::SidePanel::left("room").min_width(300.0).show(ctx, |ui| {
            ui.add_space(8.0);
            ui.label("服务器");
            ui.text_edit_singleline(&mut self.server);
            ui.add_space(6.0);
            ui.label("房间号");
            ui.text_edit_singleline(&mut self.room);
            ui.add_space(6.0);
            ui.label("房间密码");
            ui.text_edit_singleline(&mut self.password);
            ui.add_space(6.0);
            ui.label("昵称");
            ui.text_edit_singleline(&mut self.nickname);
            ui.add_space(6.0);
            ui.checkbox(&mut self.create, "创建新房间（密码必填）");
            ui.add_space(10.0);
            if ui
                .add_enabled(!self.connected, egui::Button::new("加入 / 创建"))
                .clicked()
            {
                self.connect();
            }
            ui.separator();
            ui.label("当前房间");
            if self.room_code.is_empty() {
                ui.weak("未加入");
            } else {
                ui.monospace(&self.room_code);
            }
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("成员");
            if self.members.is_empty() {
                ui.weak("暂无成员（加入房间后显示）");
            } else {
                egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                    for (nick, vip) in &self.members {
                        let name = if nick.is_empty() { "(未命名)" } else { nick.as_str() };
                        ui.horizontal(|ui| {
                            ui.label(name);
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.monospace(vip.to_string());
                            });
                        });
                    }
                });
            }
            ui.separator();
            ui.heading("传输");
            ui.weak("文件传输将在下一阶段接入进度与暂停/续传。");
            ui.separator();
            ui.heading("日志");
            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for line in &self.log {
                        ui.monospace(line);
                    }
                });
        });
    }
}
