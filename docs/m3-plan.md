# Ara M3 方案（2026-11-18）

> 状态：**方案已对齐，实现尚未开始**。本文是开发依据；实现进度见 `docs/m3-delivery-2026-11-18.md`。

## 1. 目标

在 M2 基础上把 Ara 拆成两个独立分发产品，并新增房间、验证文件、文件传输与安全层：

- `ara-client`：终端用户使用。Windows 有 GUI + CLI，Linux 只有 CLI。
- `ara-server`：部署在中继设备上。提供房间列表/成员/VIP 分配、验证文件、按房间隔离的中继、可踢房间/ID。
- 平台：Windows、Linux（**macOS 暂不做**）。共 4 个构建产物：client/server × Win/Linux。
- 验收：单房间 4 人全互通；单房间硬上限 8。

## 2. 身份与房间

- 用户 ID = 设备 `identity.key`（X25519 私钥）派生出的公钥 base64；GUI 可改昵称，昵称随注册同步（`PeerInfo.nickname`）。
- 房间 = 短码 + 创建者设置的房间密码；服务端存 salted hash，客户端不落明文。
- 房间列表对所有持验证文件的客户端可见：房间号 + 创建者昵称 + 在线人数/上限；**不能直接连接**，必须输密码加入。
- 服务端是房间成员与 VIP 分配的权威；按公钥找回原 VIP。
- **一台设备同一时刻只能在一个房间**；切换时客户端二次确认，先退出旧房（拆 TUN、清 peer 表）再进新房。
- 每个房间是独立网络，各自使用 `10.99.0.0/24`；服务端只共用中继转发，房间之间零交叉（中继按连接所属房间过滤）。

## 3. 验证文件

- 服务端部署后由 `ara-server gen-access` 生成，纯接入凭证：服务器地址 + 随机 join token（服务端本地保存比对），**不含房间信息、不含私钥**。
- 可重新生成使旧文件失效；服务端可吊销单个 token。
- 客户端导入后仅用于连上服务器、拉取房间列表；离线直连不需要验证文件。

## 4. 本地状态

- 房间快照 JSON：只在主动退出房间、断线/崩溃前写入；内容 = 房间号 + 成员 `ID → VIP + 昵称`；重连后下载最新成员表覆盖。
- **IP 直连不写 JSON、不建房间、不进列表**。
- `transfers.json`：文件传输断点状态，与房间快照、日志完全独立。
- 客户端日志默认不落盘，设置中可手动开启保存；三者不混用。

## 5. 无服务器直连

- 唯一入口：高级选项「填对方 IP:端口」；不叫「离线模式」。
- 背后复用现有 TUN + WireGuard + UDP/TCP 打洞 + 公共 STUN；**server relay 与 peer-relay 关闭**。
- 不显示房间列表、不写房间快照；VIP 在高级选项中手填，不落盘。

## 6. 文件传输

- 发送前先向房间内**单个**成员发请求：接受 / 拒绝 / 过期（60s）/ 取消 四态；只支持单播。
- 接受后建立传输会话：64KB 分片 + ACK + 已确认 offset 续传 + 整文件 SHA-256。
- 只允许发给同房间在线成员；接收端只写用户选定目录，拒绝 `../` 与绝对路径。
- 被踢/换房时暂停并保留 `transfers.json`，重连后按 file_id 续传。
- 通用 TCP 流量（SMB/共享）不承诺无损恢复：重连后 VIP 不变、UI 提示「连接已恢复，请重试未完成的传输」。

## 7. 日志与运维

- 服务端：自动写 `ara-server.log`，按天轮转、单文件 10MB、保留 7 天；脱敏密码/token/私钥，只留事件与结果；`--log-dir` 可覆盖。
- 客户端：默认只在 GUI 面板/控制台显示；设置里提供「保存日志到文件」开关。
- 服务端运维：`kick --room <code>` / `kick --id <id>`，封禁表落盘；客户端收到带 reason 的 `Kicked` 后拆 TUN、清 peer、删房间快照、弹提示、**停止自动重连**（需重新导入验证文件）。

## 8. 目录结构

```
Ara/
├── ara/                 共享 lib + CLI（Win/Linux）
│   └── src/  engine proto stun candidates config client
│             room_state.rs  transfer.rs  main.rs
├── ara-server/          服务端（Win/Linux）
│   └── src/  lib.rs rooms.rs access.rs logging.rs main.rs
├── ara-gui/             仅 Windows，egui，windows_subsystem=windows
│   └── src/  app.rs transfer_ui.rs tray.rs logging_ui.rs build.rs
├── vendor/wintun.dll    现有，Windows 专用
├── scripts/             build-release.sh / install.ps1 / install.sh（扩成 4 产物）
├── docs/                m3-plan.md / ui-guidelines.md / packaging.md / m3-delivery-*.md
└── tests/               现有 loopback / throughput + 房间/传输/安全负向用例
```

## 9. 协议改动清单

1. `Register` 加 `room_code`、`nickname`、挑战应答字段；`PeerInfo` 加 `nickname`（全部 `serde(default)`，旧 demo 仍可连）。
2. 新增消息：`Access`、`Challenge`、`RoomList`、`RoomJoin`、`Kicked`、`TransferRequest`、`TransferAccept`。
3. 中继帧格式不变；服务端按 ws 连接所属房间过滤转发，跨房间丢弃。
4. 引擎不变量不变：单循环对所有传输通道 `try_send`、满则丢包；M2 端口预测与 relay 背压回归测试继续守护；peer-relay 仅在线时可用。

## 10. 三阶段与验收

1. **各设备单机**：Win GUI/CLI、Linux CLI、server 双平台启动/安装/UAC/无 shell 窗口/日志/VIP。
2. **多设备各向连接**：4 人全互通（UDP/TCP/relay/IP 直连/文件传输/断点续传）、断线重连 VIP 不变、切换房间、被踢、房间隔离、上限 8 拒绝。
3. **安全落实**：wss（自签 + pin）、X25519 DH 挑战、nonce 一次性、密码 salted hash、token 吊销、本地 0600。

前两阶段只在测试网络跑明文原型，协议字段按终版预留，阶段 3 只填实现。

## 11. 威胁模型

| 威胁 | 对策 | 负向验证 |
|---|---|---|
| 假验证文件替换指纹（MITM） | 自签 + pin + GUI 指纹短码带外核对 | 换指纹的文件必须连不上 |
| 冒用他人公钥注册 | X25519 DH 挑战（非签名） | 冒用公钥必须被拒 |
| 重放 token/nonce | 一次性 + 时间戳 | 重放必须失败 |
| 密码经明文 ws/日志/本地文件泄露 | wss、salted hash、日志脱敏、本地不存密码 | 抓包/日志/文件均不可见 |
| 跨房间中继串网 | 服务端按房间过滤 | 跨房间帧必须丢弃 |

## 12. 工程约束

遵守 `AGENTS.md`：最小完整改动、小单元提交（`feat|fix|docs|test(scope): 动词+目标`）、提交前看 `git diff --cached`、**PR 合 main、不直推**、不改 `.gitignore`、不提交密钥/构建产物、新非平凡逻辑带回归测试、每阶段全量测试 + 本机/WSL/VPS 三方同步。
