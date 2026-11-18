#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

//! Windows GUI: room form, member list, connection state, TUN data plane and
//! log panel. Console-less by design (`windows_subsystem = "windows"`).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Duration;

use ara::client::UiEvent;
use ara::config::{AccessFile, ResolvedConfig};
use ara::proto::{PeerInfo, RoomInfo};
use eframe::egui;

fn main() -> eframe::Result<()> {
    let _instance = single_instance::SingleInstance::new("ara-gui").ok();
    if let Some(instance) = &_instance {
        if !instance.is_single() {
            return Ok(());
        }
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([900.0, 600.0])
            .with_title("Ara"),
        ..Default::default()
    };
    eframe::run_native(
        "Ara",
        options,
        Box::new(|cc| {
            let cjk = setup_fonts(&cc.egui_ctx);
            Ok(Box::new(AraApp::new(cjk)))
        }),
    )
}

fn setup_fonts(ctx: &egui::Context) -> bool {
    let mut fonts = egui::FontDefinitions::default();
    let mut paths = Vec::new();
    if let Ok(path) = std::env::var("ARA_CJK_FONT") {
        paths.push(path);
    }
    paths.extend([
        r"C:\Windows\Fonts\simhei.ttf".to_owned(),
        r"C:\Windows\Fonts\simkai.ttf".to_owned(),
        r"C:\Windows\Fonts\simfang.ttf".to_owned(),
    ]);
    let mut loaded = false;
    for path in paths {
        let Ok(data) = std::fs::read(&path) else { continue };
        fonts
            .font_data
            .insert("cjk".to_owned(), egui::FontData::from_owned(data));
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts.families.entry(family).or_default().push("cjk".to_owned());
        }
        loaded = true;
        break;
    }
    ctx.set_fonts(fonts);
    loaded
}

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Idle,
    Connecting,
    Connected,
}

enum GuiEvent {
    Ui(UiEvent),
    Failed(String),
    Rooms(Vec<RoomInfo>),
    Info(String),
}

struct Msg {
    gen: u64,
    ev: GuiEvent,
}

struct AraApp {
    server: String,
    access_path: String,
    access: Option<AccessFile>,
    room: String,
    password: String,
    nickname: String,
    create: bool,
    show_password: bool,
    direct_mode: bool,
    direct_peer: String,
    direct_key: String,
    direct_peer_vip: String,
    direct_self_vip: String,
    transfer_target: String,
    transfer_file: String,
    status: String,
    status_error: bool,
    phase: Phase,
    room_code: String,
    vip: Option<Ipv4Addr>,
    members: Vec<PeerInfo>,
    rooms: Vec<RoomInfo>,
    log: Vec<String>,
    device_id: String,
    gen: u64,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    stop_tx: Option<tokio::sync::oneshot::Sender<()>>,
    pending_switch: bool,
}

impl AraApp {
    fn new(cjk_font: bool) -> Self {
        let (tx, rx) = channel::<Msg>();
        let mut app = Self {
            server: "127.0.0.1:23947".into(),
            access_path: String::new(),
            access: None,
            room: String::new(),
            password: String::new(),
            nickname: String::new(),
            create: false,
            show_password: false,
            direct_mode: false,
            direct_peer: String::new(),
            direct_key: String::new(),
            direct_peer_vip: "10.99.0.2".into(),
            direct_self_vip: "10.99.0.1".into(),
            transfer_target: String::new(),
            transfer_file: String::new(),
            status: "未连接".into(),
            status_error: false,
            phase: Phase::Idle,
            room_code: String::new(),
            vip: None,
            members: Vec::new(),
            rooms: Vec::new(),
            log: Vec::new(),
            device_id: ara::client::own_public_key(None),
            gen: 0,
            tx,
            rx,
            stop_tx: None,
            pending_switch: false,
        };
        if !cjk_font {
            app.note("CJK font not found; set ARA_CJK_FONT");
        }
        if let Some(snapshot) = ara::room_state::load() {
            if !snapshot.room_code.trim().is_empty() {
                app.room = snapshot.room_code.clone();
                app.vip = snapshot.virtual_ip;
                app.note(format!(
                    "上次房间 {}，{} 位成员（仅作缓存，以服务器为准）",
                    snapshot.room_code,
                    snapshot.members.len()
                ));
            }
        }
        app
    }

    fn note(&mut self, s: impl Into<String>) {
        self.log.push(s.into());
        if self.log.len() > 200 {
            self.log.remove(0);
        }
    }

    fn error(&mut self, s: impl Into<String>) {
        let text = s.into();
        self.status = text.clone();
        self.status_error = true;
        self.note(text);
    }

    fn clear_error_on(&mut self, resp: &egui::Response) {
        if resp.changed() && self.status_error {
            self.status_error = false;
            if self.phase == Phase::Idle {
                self.status = "未连接".into();
            }
        }
    }

    fn load_access(&mut self) {
        let path = PathBuf::from(self.access_path.trim());
        if path.as_os_str().is_empty() {
            self.error("请先填写验证文件路径");
            return;
        }
        match AccessFile::read(&path) {
            Ok(af) => {
                self.server = af.server.clone();
                self.status_error = false;
                self.status = if af.cert_pem.is_empty() {
                    "已载入验证文件（明文 ws）".into()
                } else {
                    "已载入验证文件（WSS 加密）".into()
                };
                let s = self.status.clone();
                self.note(s);
                self.access = Some(af);
            }
            Err(e) => self.error(format!("验证文件读取失败：{e}")),
        }
    }

    fn refresh_rooms(&mut self) {
        let server = self.server.trim().to_string();
        let cert = self.access.as_ref().map(|a| a.cert_pem.clone()).unwrap_or_default();
        let token = self.access.as_ref().map(|a| a.token.clone()).unwrap_or_default();
        let server_pub = self.access.as_ref().map(|a| a.server_pub.clone()).unwrap_or_default();
        let tx = self.tx.clone();
        let gen = self.gen;
        self.note("正在获取房间列表…");
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = tx.send(Msg { gen, ev: GuiEvent::Failed(format!("runtime: {e}")) });
                    return;
                }
            };
            let ev = match rt.block_on(ara::client::fetch_rooms(&server, &cert, &token, &server_pub)) {
                Ok(rooms) => GuiEvent::Rooms(rooms),
                Err(e) => GuiEvent::Failed(format!("房间列表获取失败：{e}")),
            };
            let _ = tx.send(Msg { gen, ev });
        });
    }

    fn send_file(&mut self) {
        let Ok(vip) = self.transfer_target.trim().parse::<Ipv4Addr>() else {
            self.error("目标 VIP 无效（如 10.99.0.2）");
            return;
        };
        let file = PathBuf::from(self.transfer_file.trim());
        if !file.is_file() {
            self.error("文件不存在或不是普通文件");
            return;
        }
        let addr = SocketAddr::new(IpAddr::V4(vip), ara::transfer::TRANSFER_PORT);
        let tx = self.tx.clone();
        let gen = self.gen;
        self.note(format!("开始发送 {}", file.display()));
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = tx.send(Msg { gen, ev: GuiEvent::Info(format!("发送失败：{e}")) });
                    return;
                }
            };
            let ev = match rt.block_on(ara::transfer::send(addr, file)) {
                Ok(()) => GuiEvent::Info("发送完成，SHA-256 校验通过".into()),
                Err(e) => GuiEvent::Info(format!("发送失败：{e}")),
            };
            let _ = tx.send(Msg { gen, ev });
        });
    }

    fn disconnect(&mut self, reason: &str) {
        if !self.room_code.is_empty() && !self.direct_mode {
            let snap = ara::room_state::RoomSnapshot::from_peers(
                &self.room_code,
                self.vip,
                &self.members,
            );
            if let Err(e) = ara::room_state::save(&snap) {
                self.note(format!("保存房间快照失败：{e}"));
            }
        }
        if let Some(stop) = self.stop_tx.take() {
            let _ = stop.send(());
        }
        self.gen = self.gen.wrapping_add(1);
        self.phase = Phase::Idle;
        self.room_code.clear();
        self.vip = None;
        self.members.clear();
        self.status_error = false;
        self.status = reason.to_string();
        self.note(format!("已退出房间：{reason}"));
    }

    fn connect(&mut self) {
        if self.phase != Phase::Idle {
            return;
        }
        let server = self.server.trim().to_string();
        let (host, port) = match server.rsplit_once(':') {
            Some((h, p)) => match p.parse::<u16>() {
                Ok(v) => (h.to_string(), v),
                Err(_) => {
                    self.error("服务器端口无效");
                    return;
                }
            },
            None => {
                self.error("服务器地址需为 host:port");
                return;
            }
        };
        let cert = self.access.as_ref().map(|a| a.cert_pem.clone()).unwrap_or_default();
        let token = self.access.as_ref().map(|a| a.token.clone()).unwrap_or_default();
        let server_pub = self.access.as_ref().map(|a| a.server_pub.clone()).unwrap_or_default();
        let direct = self.direct_mode;

        let mut cfg = ResolvedConfig {
            network: String::new(),
            secret: String::new(),
            server_host: host,
            server_port: port,
            local_port: 0,
            tcp_port: 0,
            identity_path: None,
            room_code: String::new(),
            room_password: String::new(),
            access_token: token,
            server_pub,
            server_cert_pem: cert,
            nickname: self.nickname.trim().to_string(),
            create_room: false,
            direct_peer_addr: None,
            direct_peer_key: String::new(),
            direct_peer_vip: None,
            direct_vip: None,
        };

        if direct {
            let Ok(peer) = self.direct_peer.trim().parse::<SocketAddr>() else {
                self.error("对端地址无效（需 IP:端口）");
                return;
            };
            let Ok(peer_vip) = self.direct_peer_vip.trim().parse::<Ipv4Addr>() else {
                self.error("对端虚拟 IP 无效");
                return;
            };
            let Ok(own_vip) = self.direct_self_vip.trim().parse::<Ipv4Addr>() else {
                self.error("本机虚拟 IP 无效");
                return;
            };
            if self.direct_key.trim().is_empty() {
                self.error("需填写对端公钥");
                return;
            }
            cfg.direct_peer_addr = Some(peer);
            cfg.direct_peer_key = self.direct_key.trim().to_string();
            cfg.direct_peer_vip = Some(peer_vip);
            cfg.direct_vip = Some(own_vip);
        } else {
            if self.room.trim().is_empty() && !self.create {
                self.error("请填写房间号或选择创建");
                return;
            }
            if self.create && self.password.is_empty() {
                self.error("创建房间需要密码");
                return;
            }
            cfg.room_code = if self.create {
                String::new()
            } else {
                self.room.trim().to_uppercase()
            };
            cfg.room_password = self.password.clone();
            cfg.create_room = self.create;
        }

        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
        self.stop_tx = Some(stop_tx);
        let gen = self.gen;
        let tx = self.tx.clone();
        let (etx, mut erx) = tokio::sync::mpsc::unbounded_channel::<UiEvent>();
        self.phase = Phase::Connecting;
        self.status_error = false;
        self.status = "连接中…".into();
        self.note(if direct {
            "正在建立离线直连（不连服务器、不建房间、不写 JSON）".into()
        } else {
            format!("正在连接 {}:{}…", cfg.server_host, cfg.server_port)
        });

        std::thread::spawn(move || {
            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = tx.send(Msg { gen, ev: GuiEvent::Failed(format!("runtime: {e}")) });
                    return;
                }
            };
            let fwd = tx.clone();
            rt.spawn(async move {
                while let Some(ev) = erx.recv().await {
                    if fwd.send(Msg { gen, ev: GuiEvent::Ui(ev) }).is_err() {
                        break;
                    }
                }
            });
            let (from_tx, from_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
            let (to_tx, to_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
            let (assigned_tx, assigned_rx) = tokio::sync::oneshot::channel::<Ipv4Addr>();
            let done_tx = tx.clone();
            rt.spawn(async move {
                let res = if direct {
                    ara::client::run_direct(cfg, from_rx, to_tx, assigned_tx, Some(etx)).await
                } else {
                    ara::client::run(cfg, from_rx, to_tx, assigned_tx, Some(etx)).await
                };
                if let Err(e) = res {
                    let _ = done_tx
                        .send(Msg { gen, ev: GuiEvent::Failed(format!("连接终止：{e}")) });
                }
            });
            let vip = rt.block_on(async {
                tokio::select! {
                    _ = &mut stop_rx => None,
                    r = tokio::time::timeout(Duration::from_secs(20), assigned_rx) => {
                        r.ok().and_then(|x| x.ok())
                    }
                }
            });
            match vip {
                Some(vip) => {
                    if let Err(e) = rt.block_on(tun_up(vip, from_tx, to_rx)) {
                        let _ = tx.send(Msg {
                            gen,
                            ev: GuiEvent::Ui(UiEvent::Status(format!(
                                "TUN 创建失败（需管理员权限）：{e}"
                            ))),
                        });
                    }
                }
                None => {
                    let _ = tx.send(Msg {
                        gen,
                        ev: GuiEvent::Failed("加入房间超时或被拒绝".into()),
                    });
                }
            }
            rt.block_on(async {
                tokio::select! {
                    _ = &mut stop_rx => {},
                    _ = tokio::time::sleep(Duration::from_secs(3600)) => {},
                }
            });
        });
    }

    fn poll(&mut self) {
        let mut msgs = Vec::new();
        while let Ok(m) = self.rx.try_recv() {
            msgs.push(m);
        }
        for m in msgs {
            if m.gen != self.gen {
                continue;
            }
            match m.ev {
                GuiEvent::Ui(UiEvent::Welcome { virtual_ip, room_code }) => {
                    self.vip = Some(virtual_ip);
                    self.phase = Phase::Connected;
                    self.status_error = false;
                    if room_code.is_empty() {
                        self.status = format!("离线直连已建立，VIP {virtual_ip}");
                    } else {
                        self.room_code = room_code.clone();
                        self.room = room_code.clone();
                        self.status = format!("已连接房间 {room_code}，VIP {virtual_ip}");
                    }
                    let s = self.status.clone();
                    self.note(s);
                }
                GuiEvent::Ui(UiEvent::Peers(peers)) => {
                    self.note(format!("成员更新：{} 人", peers.len()));
                    if !self.direct_mode && !self.room_code.is_empty() {
                        let snap = ara::room_state::RoomSnapshot::from_peers(
                            &self.room_code,
                            self.vip,
                            &peers,
                        );
                        let _ = ara::room_state::save(&snap);
                    }
                    self.members = peers;
                }
                GuiEvent::Ui(UiEvent::Kicked(reason)) => {
                    self.disconnect("已被服务器移出");
                    self.error(format!("被服务器移出：{reason}"));
                }
                GuiEvent::Ui(UiEvent::Status(s)) => {
                    self.status = s.clone();
                    self.status_error = false;
                    self.note(s);
                }
                GuiEvent::Failed(s) => {
                    if let Some(stop) = self.stop_tx.take() {
                        let _ = stop.send(());
                    }
                    self.gen = self.gen.wrapping_add(1);
                    self.phase = Phase::Idle;
                    self.room_code.clear();
                    self.vip = None;
                    self.members.clear();
                    self.error(s);
                }
                GuiEvent::Rooms(rooms) => {
                    self.note(format!("房间列表：{} 个房间", rooms.len()));
                    self.rooms = rooms;
                }
                GuiEvent::Info(s) => self.note(s),
            }
        }
    }
}

async fn tun_up(
    vip: Ipv4Addr,
    from_tun_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    mut to_tun_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
) -> anyhow::Result<()> {
    use futures_util::{SinkExt, StreamExt};

    #[cfg(target_os = "windows")]
    ensure_wintun()?;
    let mut config = tun::configure();
    config
        .name("ara")
        .address(vip)
        .netmask(prefix_mask(ara::VIRTUAL_PREFIX))
        .mtu(ara::TUN_MTU as i32)
        .layer(tun::Layer::L3)
        .up();
    let device = tun::create_as_async(&config)?;
    let (mut sink, mut stream) = device.into_framed().split();
    tokio::spawn(async move {
        while let Some(pkt) = stream.next().await {
            match pkt {
                Ok(p) => {
                    if from_tun_tx.send(p.get_bytes().to_vec()).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    tokio::spawn(async move {
        while let Some(bytes) = to_tun_rx.recv().await {
            if sink.send(tun::TunPacket::new(bytes)).await.is_err() {
                break;
            }
        }
    });
    Ok(())
}

fn prefix_mask(prefix: u8) -> Ipv4Addr {
    let mask: u32 = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
    Ipv4Addr::from(mask)
}

#[cfg(target_os = "windows")]
fn ensure_wintun() -> anyhow::Result<()> {
    const DLL: &[u8] = include_bytes!("../../ara/vendor/wintun.dll");
    let exe = std::env::current_exe()?;
    let dll = exe
        .parent()
        .map(|p| p.join("wintun.dll"))
        .ok_or_else(|| anyhow::anyhow!("exe dir"))?;
    if !dll.exists() {
        std::fs::write(&dll, DLL)?;
    }
    Ok(())
}

impl eframe::App for AraApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll();
        ctx.request_repaint_after(Duration::from_millis(250));

        let dropped = ctx.input_mut(|i| std::mem::take(&mut i.raw.dropped_files));
        if let Some(path) = dropped.iter().find_map(|f| f.path.as_ref()) {
            self.access_path = path.to_string_lossy().into_owned();
            self.load_access();
        }

        egui::TopBottomPanel::top("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Ara");
                ui.separator();
                let color = if self.status_error {
                    egui::Color32::from_rgb(190, 45, 45)
                } else if self.phase == Phase::Connected {
                    egui::Color32::from_rgb(30, 135, 85)
                } else if self.phase == Phase::Connecting {
                    egui::Color32::from_rgb(185, 125, 20)
                } else {
                    egui::Color32::from_rgb(120, 120, 120)
                };
                ui.colored_label(color, &self.status);
                if let Some(vip) = self.vip {
                    ui.separator();
                    ui.label(format!("VIP {vip}"));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let short: String = self.device_id.chars().take(8).collect();
                    ui.weak(format!("设备 ID {short}…"));
                });
            });
        });

        egui::SidePanel::left("room").min_width(340.0).show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.add_space(6.0);
                ui.label("服务器 host:port");
                let server_resp = ui.add_enabled(
                    self.access.is_none(),
                    egui::TextEdit::singleline(&mut self.server),
                );
                self.clear_error_on(&server_resp);
                ui.add_space(4.0);
                ui.label("验证文件（服务端 gen-access 生成）");
                ui.horizontal(|ui| {
                    let access_resp = ui.add(
                        egui::TextEdit::singleline(&mut self.access_path)
                            .desired_width(200.0)
                            .hint_text("access-file.json"),
                    );
                    self.clear_error_on(&access_resp);
                    if ui.button("载入").clicked() {
                        self.load_access();
                    }
                    if ui.button("浏览").clicked() {
                        if let Some(path) = rfd::FileDialog::new()
                            .add_filter("Ara 验证文件", &["json"])
                            .pick_file()
                        {
                            self.access_path = path.to_string_lossy().into_owned();
                            self.load_access();
                        }
                    }
                    if self.access.is_some() && ui.button("清除").clicked() {
                        self.access = None;
                        self.note("已清除验证文件");
                    }
                });
                if let Some(af) = &self.access {
                    ui.weak(if af.cert_pem.is_empty() {
                        "连接模式：明文 ws"
                    } else {
                        "连接模式：WSS（证书固定）"
                    });
                }
                ui.add_space(6.0);
                ui.label("房间号");
                let room_resp = ui.text_edit_singleline(&mut self.room);
                self.clear_error_on(&room_resp);
                ui.label("房间密码");
                ui.horizontal(|ui| {
                    let pw_resp = ui.add(
                        egui::TextEdit::singleline(&mut self.password)
                            .password(!self.show_password)
                            .desired_width(200.0),
                    );
                    self.clear_error_on(&pw_resp);
                    ui.checkbox(&mut self.show_password, "显示");
                });
                ui.label("昵称");
                let nick_resp = ui.text_edit_singleline(&mut self.nickname);
                self.clear_error_on(&nick_resp);
                ui.add_space(4.0);
                let create_resp = ui.checkbox(&mut self.create, "创建新房间（密码必填）");
                self.clear_error_on(&create_resp);
                ui.add_space(6.0);
                let label = if self.direct_mode {
                    "离线直连"
                } else if self.phase == Phase::Connected {
                    "切换 / 加入房间"
                } else if self.phase == Phase::Connecting {
                    "连接中…"
                } else {
                    "加入 / 创建"
                };
                if ui
                    .add_enabled(self.phase != Phase::Connecting, egui::Button::new(label))
                    .clicked()
                {
                    if self.phase == Phase::Connected {
                        self.pending_switch = true;
                    } else {
                        self.connect();
                    }
                }
                ui.horizontal(|ui| {
                    if self.phase != Phase::Idle && ui.button("退出房间").clicked() {
                        self.disconnect("已主动退出房间");
                    }
                    if ui.button("刷新房间列表").clicked() {
                        self.refresh_rooms();
                    }
                });
                ui.separator();
                ui.collapsing("高级选项：无服务器直连", |ui| {
                    ui.checkbox(&mut self.direct_mode, "仅用 IP 直连对端");
                    ui.label("对端地址 IP:端口");
                    ui.text_edit_singleline(&mut self.direct_peer);
                    ui.label("对端公钥（设备 ID）");
                    ui.text_edit_singleline(&mut self.direct_key);
                    ui.label("对端虚拟 IP");
                    ui.text_edit_singleline(&mut self.direct_peer_vip);
                    ui.label("本机虚拟 IP");
                    ui.text_edit_singleline(&mut self.direct_self_vip);
                    ui.weak("离线直连不连服务器、不建房间、不写本地 JSON。");
                });
                ui.separator();
                ui.label("当前房间");
                if self.room_code.is_empty() {
                    ui.weak("未加入");
                } else {
                    ui.monospace(&self.room_code);
                }
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.heading("房间列表");
                ui.weak("只显示房间号、创建者昵称、在线人数；进入需密码二次验证。");
                if self.rooms.is_empty() {
                    ui.weak("暂无房间，点「刷新房间列表」获取。");
                } else {
                    let rooms = self.rooms.clone();
                    egui::Grid::new("room_grid").striped(true).show(ui, |ui| {
                        ui.strong("房间号");
                        ui.strong("创建者");
                        ui.strong("人数");
                        ui.end_row();
                        for r in &rooms {
                            if ui.selectable_label(false, r.code.as_str()).clicked() {
                                self.room = r.code.clone();
                                self.create = false;
                                self.note(format!("已选择房间 {}，请输入密码后加入", r.code));
                            }
                            ui.label(r.creator.as_str());
                            ui.label(format!("{}/{}", r.members, r.limit));
                            ui.end_row();
                        }
                    });
                }
                ui.separator();
                ui.heading("成员");
                if self.members.is_empty() {
                    ui.weak("暂无成员（加入房间后显示）");
                } else {
                    let members = self.members.clone();
                    for p in &members {
                        let name = if p.nickname.is_empty() {
                            "(未命名)"
                        } else {
                            p.nickname.as_str()
                        };
                        ui.horizontal(|ui| {
                            ui.label(name);
                            ui.monospace(p.virtual_ip.to_string());
                        });
                    }
                }
                ui.separator();
                ui.heading("文件传输");
                ui.horizontal(|ui| {
                    ui.label("目标 VIP");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.transfer_target)
                            .desired_width(140.0)
                            .hint_text("10.99.0.2"),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label("文件路径");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.transfer_file)
                            .desired_width(300.0),
                    );
                });
                if ui.button("发送文件").clicked() {
                    self.send_file();
                }
                ui.weak("内置断点续传；接收端运行 ara recv。交互式接收确认弹窗待接入。");
                ui.separator();
                ui.heading("日志");
                let log = self.log.clone();
                egui::ScrollArea::vertical()
                    .max_height(180.0)
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for line in &log {
                            ui.monospace(line);
                        }
                    });
            });
        });

        if self.pending_switch {
            egui::Window::new("切换房间")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.label("你当前已在房间中。切换会先退出旧房间，再连接新房间。");
                    ui.horizontal(|ui| {
                        if ui.button("确认切换").clicked() {
                            self.pending_switch = false;
                            self.disconnect("正在切换房间");
                            self.connect();
                        }
                        if ui.button("取消").clicked() {
                            self.pending_switch = false;
                        }
                    });
                });
        }
    }
}
