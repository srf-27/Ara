# Ara

自研 P2P 虚拟局域网工具：把分布在不同 NAT 后的设备组成同一个三层内网（任意协议互访、传文件、局域网游戏）。Rust 实现，Windows / Linux。M1/M2 为 CLI；M3 起 Windows 提供 GUI，Linux 保持 CLI。

数据面 WireGuard 加密，路径优先级：**UDP 直连/打洞（IPv6 优先）→ TCP 直连 → peer 一跳中继 → 服务器中继兜底**。一台 1~3M 带宽的小设备只做控制面（信令 + STUN + 打洞协调）+ 中继兜底，数据面尽量直连。

## 架构

```
小服务器（ara-server）          客户端（ara）
  ├─ WebSocket 信令              ├─ TUN 虚拟网卡（wintun / /dev/net/tun）
  ├─ STUN（NAT 映射探测）        ├─ boringtun（每 peer 一条 WireGuard 隧道）
  └─ 虚拟 IP 分配 / 在线管理     └─ 打洞：向对方全部候选地址发 WG 握手
```

- 虚拟网段 `10.99.0.0/24`，按注册顺序分配，按公钥找回原 IP；单房间硬上限 8。
- 入网模型（M1/M2）：网络名 + 共享密钥；M3 改为房间短码 + 房间密码 + 服务端验证文件（见 `docs/m3-plan.md`）。
- 设备本地生成 WireGuard 密钥对；ID 即公钥。
- 单 UDP socket 共用 STUN 与 WireGuard（默认端口 23947），STUN 反射地址即 WG 真实映射。
- 保活 25s；信令断线 1~30s 指数退避重连；心跳 15s，服务器 45s 判离线。

## 构建

```sh
cargo build --release --workspace
```

产物：`target/release/ara`（客户端）、`target/release/ara-server`（服务器）。Windows 客户端运行时需管理员权限（wintun），Linux 需 root。

## 使用

服务器（公网机器，放行 TCP+UDP 23947）：

```sh
./ara-server --port 23947
```

客户端（每台设备，network/secret 一致）：

```sh
./ara up --network demo --secret <密钥> --server <服务器IP>:23947
```

或用配置文件 `ara.toml`（默认读当前目录，`--config` 可指定，命令行参数覆盖文件）：

```toml
network = "demo"
secret = "your-secret"
server = "1.2.3.4:23947"
# port = 23947   # 本地 UDP 端口，0 = 随机
```

启动后各设备获得 `10.99.0.x` 虚拟 IP，直接互 ping / 访问任意服务。Ctrl+C 退出并清理网卡。

> 安全提示：M1/M2 的 `secret` 以明文存于配置文件，信令为明文 ws；M3 将改为 wss + 验证文件 + DH 挑战（见 `docs/m3-plan.md` §11）。当前版本请勿在不可信网络公开分享配置。

## 测试

```sh
cargo test --workspace
```

含 server + 2 逻辑客户端的 loopback 集成测试（注册→打洞→加密互通，不建 TUN）与 `#[ignore]` 吞吐诊断。M3 方案见 [`docs/m3-plan.md`](docs/m3-plan.md)，GUI 规范见 [`docs/ui-guidelines.md`](docs/ui-guidelines.md)。

## 范围与限制

- 已实现（M1/M2）：注册 / 稳定虚拟 IP / STUN / IPv6 与 LAN 直连 / 锥型 NAT 打洞 / 对称 NAT 端口预测 / TCP 直连 / peer 一跳中继 / 服务器中继兜底 / 背压不死锁 / 发布安装脚本。
- M3 规划：房间短码 + 密码 + 验证文件、Windows GUI、内置文件传输（含断点续传）、wss 与身份挑战。
- 限制：仅 L3，无二层广播发现；双对称 NAT 且无 IPv6 时可能退中继；macOS 暂不支持。