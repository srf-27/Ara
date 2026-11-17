# Ara 交付文档 / Agent 接手指南

> 最后更新：2026-11-18。本文档供接手的 agent 快速继续开发。先读 §1–§3 了解全貌，§5 记录已修复的两个死锁 bug（§5.1–5.5 TCP 握手死锁、§5.6 relay 负载下引擎单循环阻塞死锁，均含根因 + 修复 + 验证），§7 是环境陷阱（务必先看，否则会浪费大量时间），§10 是 M2 剩余范围与优先级。

## 1. 项目目标

Ara 是一个 P2P 虚拟局域网工具：把分布在各地的设备（Windows / Linux）打成同一个三层内网（虚拟网段 `10.99.0.0/24`），任意设备互访、任意协议（ping / SMB / MC 联机 / 传文件），像真局域网。

核心架构承诺：**全 client 对等**，每台设备既是客户端也是潜在服务端。路径优先级：
1. **UDP 直连 / 打洞**（WireGuard over UDP，IPv6 优先）
2. **TCP 直连**（UDP 被封时，peer 间直接 TCP 跑 WireGuard，不经服务器）
3. **服务器中继兜底**（前两者都不通时，经信令 TCP 连接转发 WG 包）

设计要点：小服务器（1C1G30M）只做控制面（信令 + STUN + 打洞协调 + 中继兜底），数据面尽量直连。不改 DNS、不劫持默认路由（split-tunnel，仅虚拟网段进隧道）、仅占用指定端口。

## 2. 仓库 / 分支 / 构建状态

- 项目根：`D:\repo-REPO\Ara`
- 远程：`https://github.com/srf-27/Ara.git`，默认分支 **main**（AGENTS.md 写 master，但仓库既有策略优先 → 用 main）
- 当前分支：`feature/m1-scaffold`（叠在 `feature/agents-md` 上）
- 已推送的最新提交：`c1dc278`（TCP 直连传输，当时 gated 关闭）；其后本地提交了 TCP 死锁修复（§5，启用 `TCP_DIRECT`）
- cargo workspace 两 crate：`ara`（客户端 CLI + 共享协议）、`ara-server`（信令/STUN/中继）
- 语言 Rust，tokio 异步，boringtun（WireGuard 用户态）+ tun crate（wintun / /dev/net/tun）
- `gh` 未登录 → PR 建不了，需用户 `gh auth login` 后建 PR 合 main，或手动合并

## 3. 已完成并验证的能力

| 能力 | 状态 | 验证方式 |
|---|---|---|
| 注册 / 虚拟 IP 分配 / STUN | ✅ | 真机 + 测试 |
| IPv6/LAN/锥型 UDP 打洞 | ✅ | loopback 集成测试 |
| WireGuard TUN 数据面 | ✅ | 真机互 ping |
| 服务器中继兜底（relay） | ✅ | 真机（校园网封 UDP 时自动走 relay） |
| 稳定 IP（密钥落盘 + 服务器分配表落盘，从 .1 起编） | ✅ | VPS 重启拿回 .1 |
| 候选地址周期刷新（30s） | ✅ | 修好换网后候选过期 |
| MTU 1280（修校园网 PMTU 黑洞） | ✅ | 真机大文件传输 |
| STUN 回退链（服务器→公共 STUN 腾讯/小米） | ✅ | 服务器 TCP-only 时 |
| 重连 peer 表全量对齐（SetPeers） | ✅ | 修好残留死 peer |
| **TCP 直连传输** | ✅ | loopback 集成测试 + 真机（见 §5） |
| 僵尸 tcp_tx 自愈 + relay 无缝兜底 | ✅ | 真机验证（见 §5） |
| **relay 负载不死锁**（引擎循环非阻塞） | ✅ | 回归测试 + 真机 iperf TCP/UDP（见 §5.6） |

**真机验收成果（本轮）**：
- 六向 ping 全通（0% 丢包）。
- **Win↔WSL TCP 直连：270 Mbps（33.7 MB/s），2ms 延迟**——证明 TCP 直连端到端跑满带宽。
- 黑洞路径（Win↔VPS）自动走 relay，**不再 100% 丢包**（修复前僵尸 tcp_tx 会杀死连通性）。
- 副作用核查通过：不改 DNS、不劫持默认路由、仅虚拟网段进隧道、仅指定端口通信。

**测试**：`ara` lib 13 个 + `ara-server` lib 5 个 + 集成 4 个默认跑（`two_clients_exchange_ip_packets` UDP、`two_clients_exchange_ip_packets_over_tcp`、`relay_routes_binary_frames`、`relay_backpressure_keeps_engine_responsive`）+ 2 个 `#[ignore]` 吞吐诊断，**全绿**；`cargo clippy --workspace --all-targets` 无警告。TCP 死锁修复见 §5，relay 负载死锁修复见 §5.6。

## 4. 关键代码地图（`ara/src/engine.rs`）

数据面是单任务事件循环 `Engine::run`（约 line 200），拥有共享 UDP socket + 每 peer 一条 boringtun `Tunn`。

- `Command` 枚举（line ~41）：`AddPeer/SetPeers/RemovePeer/Stun/RelayIn/TcpUp/TcpAccepted/TcpIn/TcpClosed`
- `Peer` 结构（line ~151）：新增字段 `tcp_proven: bool`（TCP 是否已被收到的 WG 包证明可用）
- `send_tcp()`（line ~84）：非阻塞 TCP 发送助手，`try_send`；Closed 清 tcp_tx 回退 relay，Full 跳过保留 sender（§5.6）
- `apply()`（line ~104）：路由 WG 出站包。proven 只走 TCP；未证明时 TCP probe 叠加 relay/UDP 兜底（§5.3）；tcp_tx/relay_out/ip_in 全 `try_send` 不阻塞循环（§5.6）
- `punch()`（line ~156）：发 WG handshake initiation（UDP + TCP probe + relay），同样非阻塞
- `attach_tcp()`（line ~352）：把 TCP stream 拆成 reader/writer 任务；outbound 设 `peer.tcp_tx`，inbound 存 `tcp_writers[conn]`
- `ingest_tcp()`（line ~405）：收 TCP 上的 WG 包，匹配 peer，**成功 decap 即设 `tcp_proven=true, use_relay=false`**
- `on_tick()`（line ~537）：relay 激活判定（line ~553）+ TCP 连接尝试（line ~562）+ `update_timers`
- `TCP_DIRECT` 常量（line ~22）：现为 `true`（已启用）
- 常量：`BUF=2048, TICK=250ms, PUNCH_INTERVAL=2s, STUN_TIMEOUT=3s`

TCP 帧格式：2 字节大端长度 + WG 包体。reader 读帧→`TcpIn{conn,key,data}`；writer 从 mpsc 收包→写帧。

## 5. TCP 直连死锁 bug（已修复，供回归排查）

### 5.1 症状

启用 `TCP_DIRECT` 后，`two_clients_exchange_ip_packets_over_tcp` 超时（30s）：A attach 了 outbound TCP，但 B 从未收到 A 的握手包，A 也打不出 TCP 上的 initiation。真机 Win↔WSL 偶尔 ~2min 才通。

### 5.2 根因（鸡生蛋，已对照 boringtun 0.6.0 源码核实）

- `tcp_proven` 门控要求「收到对端 WG 包」才走 TCP；但初始 `tcp_proven=false`。
- WG initiation 主要由 `on_tick` 的 `update_timers()` 产出，经 `apply()` 路由。旧 `apply()` 在 `tcp_proven=false` 时只走 UDP 候选（测试里是黑洞 `127.0.0.1:1`）或 relay（测试里被 drain），**从不走 TCP** → initiation 到不了对端 → 永远无法 prove → 死锁。
- `punch()` 本想经 tcp_tx 发 initiation 来 prove，但 boringtun `format_handshake_initiation` 在 `handshake.is_in_progress()` 时返回 `Done`（`noise/mod.rs`）：`update_timers` 已经启动了握手、消耗了 initiation，`punch` 拿不到包。
- `update_timers` 每 `REKEY_TIMEOUT`（5s）重发一次 initiation（`noise/timers.rs`），所以只要 `apply()` 愿意经 TCP 发，5s 内就能 prove。

### 5.3 修复（已实现并验证）

1. **`apply()` 未证明时 TCP probe 叠加兜底**：`tcp_proven=true` 只走 TCP；否则只要 `tcp_tx` 存在就把包**克隆一份经 TCP probe 发出**（不 return），再叠加 relay/UDP 兜底。initiation 经 TCP 到达对端 → 对端回应 → `ingest_tcp` 设 proven → 之后只走 TCP。WG 按计数器去重，多路径发同一包安全；黑洞路径下 probe 无回应，relay 保持连通。
2. **`tcp_proven` 门控**（防黑洞 TCP 切断连通性）：attach 不再关 relay；只有 `ingest_tcp` 成功 decap 才设 `tcp_proven=true, use_relay=false`。NAT SYN 代理让 `connect()`「成功」但 payload 被黑洞时，relay 不被切断。
3. **僵尸 tcp_tx 自愈**：writer 任务退出时发 `TcpClosed(conn)`；`apply()` 开头检测 `tx.is_closed()` 即清 tcp_tx、`tcp_proven=false`、`use_relay=true`；`TcpClosed` handler 同样清理。
4. **relay 激活条件**（`on_tick`）：用 `!tcp_proven` 而非 `tcp_tx.is_none()`，否则 probing 期间 relay 永不激活。

### 5.4 验证（完整门禁，勿只跑单个测试）

```sh
cargo test -p ara --test loopback two_clients_exchange_ip_packets_over_tcp   # 死锁回归，应 ~5s 通过
cargo test -p ara --lib && cargo test -p ara --test loopback && cargo test -p ara-server --lib  # 全量，确认 UDP/relay 无回归
cargo clippy --workspace --all-targets                                       # 应无警告
```

结果：21 个测试全绿（TCP 测试 5s 通过），clippy 无警告。**真机复核仍待做**：部署三机确认 Win↔WSL TCP 直连仍 ~270Mbps、Win↔VPS 黑洞路径 relay 兜底仍通（见 §8）。

### 5.5 日志约定

每包噪声日志已删（`ingest_tcp` 入口、`punch via tcp`）；保留连接级 debug（attach/reader/writer/TcpClosed）与 proven/established 的 info。新增日志勿按包打，避免高吞吐下刷屏。

### 5.6 引擎单循环阻塞死锁（真机 relay 复测发现，已修复）

**症状**：WSL↔VPS relay 静止时互 ping 正常（RTT ~322ms），但 iperf3 负载后**双向 100% 丢包**；VPS 引擎刷 `REKEY_TIMEOUT`/`CONNECTION_EXPIRED`，WSL 引擎负载后**零日志**（单循环卡死）。TCP iperf sender 512KB / receiver 0、cwnd 崩到 1 MSS；UDP iperf 挂起。

**根因**：引擎是单事件循环，`apply()`/`punch()` 对**有界传输通道用阻塞 `.await`**：
- `tcp_tx`（容量 256）：writer 任务 `write_all().await` 在被黑洞的 TCP（NAT SYN 代理，connect「成功」但 payload 丢）上内核缓冲填满后**永久 pending**，writer 停止排空 → 通道满 → `tx.send().await` 卡死整个循环。§5.3 的 TCP probe 把每包克隆到 TCP，负载下 <1s 填满。
- `relay_out`（容量 256）：出口带宽饱和时 `relay_out.send().await` 阻塞循环 → 无法处理入站 ACK → relay 上的头阻塞 meltdown。

**修复**（根因一次修在共享代码）：
1. `send_tcp()` 助手 + `apply()`/`punch()` 对 tcp_tx 改 `try_send`：`Closed`→清 tcp_tx 回退 relay；`Full`→跳过该包但保留 sender（由 writer 超时回收）。引擎循环永不阻塞在 TCP。
2. `relay_out`/`ip_in` 改 `try_send`：满则丢包（WG/上层重传），循环保持响应入站。饱和丢包触发 TCP 拥塞退避而非死锁。
3. TCP writer `write_all` 加 **5s 超时**：黑洞 TCP 停滞即报错退出 → `TcpClosed` → 回退 relay 并释放 socket。
4. TCP reader 首帧读加 **60s 空闲超时**：回收黑洞读半侧（健康 peer ≤25s keepalive，不会误杀）。

**验证**：
- 回归测试 `relay_backpressure_keeps_engine_responsive`（默认跑，~14s）：暂停排空 A 的 relay_rx（通道保活）灌包填满 relay_out，断言引擎仍响应 `Stun` 命令、恢复排空后继续投递。**修复前失败（引擎卡死），修复后通过**。
- 真机 WSL↔VPS relay：TCP iperf 15s sender 5.5MB/3.08Mbps、receiver 4.5MB/2.33Mbps、**0 重传**、cwnd 106KB→914KB 健康增长；UDP iperf 10s@1M **0% 丢包**；负载前后 ping 0% 丢包、引擎计时正常。死锁根除。
- ~3Mbps 是 WAN relay 带宽上限（校园网→境外 VPS、322ms RTT、TCP-over-TCP），非代码瓶颈。

## 6. 测试与构建命令

```powershell
# 工具链：stable 1.100.0（本轮重装修复过，见 §7）。cargo 在 PATH 丢失时用全路径：
& "$env:USERPROFILE\.cargo\bin\cargo.exe" <args>

# 不碰被锁 bin exe 的测试（推荐，客户端运行时也能跑）：
cargo test -p ara --lib
cargo test -p ara --test loopback
cargo test -p ara-server --lib

# 全量（需先杀运行中的 ara.exe，否则 bin 链接被锁）：
cargo test --workspace
cargo build --workspace          # debug 产物 target\debug\ara.exe / ara-server.exe
cargo clippy --workspace --all-targets
```

Linux musl 静态二进制（在 WSL Ubuntu 里编，部署 VPS 用）：
```bash
# WSL: ~/ara 是源码副本（/mnt/d 上编译太慢，务必拷到 ~/ara）
cp /mnt/d/repo-REPO/Ara/ara/src/engine.rs ~/ara/ara/src/   # 同步改动
cd ~/ara
cargo build -p ara                                          # WSL 本机 debug
cargo build --release -p ara --target x86_64-unknown-linux-musl
cp target/x86_64-unknown-linux-musl/release/ara /mnt/d/repo-REPO/Ara/target/ara-linux
```

## 7. 环境陷阱（务必先看）

1. **exe 被运行中的客户端锁住**：`cargo build`/`test --workspace` 会因 `target\debug\ara.exe` 被占用而链接失败（os error 5）。必须先杀进程。客户端以**管理员**运行，普通 `Stop-Process -Name ara` 可能杀不掉 → **按 Id 强杀**：`Get-Process ara | ForEach-Object { Stop-Process -Id $_.Id -Force }`（需提权）。
2. **提权脚本环境差异**：`Start-Process powershell -Verb RunAs` 跑在 Administrator 上下文，rustup 默认工具链/PATH 与当前用户不同，可能报 "no default configured"。脚本里显式设 `$env:RUSTUP_TOOLCHAIN="stable"` 或用全路径 cargo。本轮已把当前用户默认工具链设为 stable（1.100.0）。
3. **工具链曾损坏**：stable 的 bin/lib 目录丢失（只剩 share），`rustup toolchain uninstall stable` 再 `install stable --profile minimal` 修复（走镜像 `RUSTUP_DIST_SERVER=https://rsproxy.cn`）。
4. **校园网封 UDP 出口**：用户当前校园网（移动 NAT，公网出口会变，如 39.144.x / 117.147.x）封 UDP，STUN/打洞全失败，TCP 正常。这是 relay 兜底和 TCP 直连的目标场景。
5. **UCloud 轻量服务器在云 NAT 后**：VPS 网卡是私网 IP（10.41.175.215），公网 128.14.237.95 经云 NAT。**TCP 23948 端口数据被黑洞**（SYN 能过、connect「成功」，但 payload 不到，tcpdump 0 包）——疑似 NAT SYN 代理。UDP 23947 正常、UDP 23948 也曾被挡。控制台防火墙规则正确但端口行为怪异。**不要在这条路径上纠结防火墙，是 provider 问题。**
6. **同机 server/client 端口冲突**：已移除 `SO_REUSEADDR`，冲突会报错。同机自测客户端用 `--port 0`（随机 UDP）。
7. **WSL DNS**：WSL 默认 DNS 走宿主隧道，校园网解析境外域名失败。已固化 `/etc/wsl.conf` `generateResolvConf=false` + `/etc/resolv.conf` 用 223.5.5.5/119.29.29.29。
8. **clippy 组件与工具链不同步**：stable 用 `--profile minimal` 重装后不含 clippy，`cargo clippy` 会落到旧版（如 0.1.99）并报 E0514「crate compiled by incompatible rustc」。修复：删工具链目录里的陈旧 clippy 残留（`bin/cargo-clippy.exe`、`bin/clippy-driver.exe` 及 `.pdb`、`share/doc/clippy`、`share/doc/rust/html/clippy`、`lib/rustlib/manifest-clippy-preview-*`），再 `rustup component add clippy`。装好后 `cargo clippy --version` 应与 `rustc --version` 一致。

## 8. 部署拓扑（三机实测环境，当前运行中）

| 角色 | 虚拟 IP | 说明 |
|---|---|---|
| VPS（控制面 + peer） | 10.99.0.1 | CentOS Stream 9，1C764M。`ssh -i "C:\Users\Lenovo\.ssh\id_rsa" root@128.14.237.95`。ara-server 监听 TCP+UDP 23947（`/usr/local/bin/ara-server`，日志 `/root/ara-server.log`，状态 `/root/ara-state.json`）；ara client `--port 23947 --tcp-port 23948`（日志 `/root/ara-client.log`） |
| Windows 宿主 | 10.99.0.2 | 管理员运行 `ara.exe up --network demo --secret test --server 128.14.237.95:23947 --port 0`。日志 `D:\repo-REPO\Ara\target\smoke\clientA.log` |
| WSL2 Ubuntu | 10.99.0.3 | `wsl -d Ubuntu -u root`，`~/ara/target/debug/ara up --network demo --secret test --server 128.14.237.95:23947`。日志 `/root/clientB.log`。宿主 IP 在 WSL 里 = `ip route show default` 网关（172.22.176.1） |

- 网络名 `demo`，密钥 `test`（测试值，正式用换强密钥）。
- VPS 控制台防火墙已放 TCP/UDP 23947、TCP/UDP 23948。
- 重启 VPS client：`pkill -x ara; sleep 1; setsid nohup /usr/local/bin/ara up ... </dev/null >/root/ara-client.log 2>&1 &`
- 部署新二进制到 VPS：先 `pkill -x ara`（否则 scp 覆盖被占用的二进制失败），再 scp `target\ara-linux` → `/usr/local/bin/ara`。
- 测试文件清理约定：本地进回收站（host_safe_delete），WSL/VPS 移入 `/root/.ara-test-trash`（可恢复）。

## 9. Windows 重编重启的可靠模式

提权脚本曾放在 `C:\Users\Lenovo\AppData\Local\Temp\qwenwork\`（临时目录，可能已清理，勿依赖具体文件）。可靠步骤固定为：

1. **按 Id 强杀** ara（管理员运行，`Stop-Process -Name` 不可靠）：`Get-Process ara | ForEach-Object { Stop-Process -Id $_.Id -Force }`。
2. `cargo build -p ara`，校验 exe 时间戳确认重编成功。
3. 重启客户端（`RUST_LOG=ara=info`）。

提权上下文（`Start-Process powershell -Verb RunAs`，用户需点 UAC「是」）里 rustup 默认工具链/PATH 可能不同：脚本显式设 `$env:RUSTUP_TOOLCHAIN="stable"` 或用全路径 cargo（见 §7.2）。

## 10. 剩余工作（M2）

按 AGENTS.md「最小完整改动」原则：每项独立分支、先做最小可验证步骤，勿一次性堆功能。

1. ✅ **TCP 测试回归**：已修复并全量验证（§5）。
2. ✅ **relay 兜底可用性**（真机复测发现并修复死锁，见 §5.6）：
   - 真机受控复测（WSL↔VPS relay）**推翻了「代码健康、纯 TCP-over-TCP meltdown」的旧结论**：静止互 ping 正常，但 iperf 负载后双向 100% 丢包、引擎单循环卡死。根因是 `apply()`/`punch()` 对有界 tcp_tx/relay_out 用阻塞 `.await`，黑洞 TCP 或饱和 relay 会冻住整个事件循环（§5.6）。
   - 已修：tcp_tx/relay_out/ip_in 全改 `try_send`（满则丢包，循环永不阻塞）+ TCP writer 5s 写超时 + reader 60s 空闲超时。回归测试 `relay_backpressure_keeps_engine_responsive` 守护。
   - 复测成果：TCP iperf 15s ≈ 2.3–3 Mbps、**0 重传**、cwnd 健康增长；UDP iperf 1M **0% 丢包**；负载前后 ping 0% 丢包。~3Mbps 是 WAN relay 带宽上限（322ms RTT、TCP-over-TCP），非代码瓶颈。relay 仅兜底，主路径仍是 TCP/UDP 直连。
   - loopback 吞吐诊断（`relay_throughput_loopback` 裸 ws ≈ 50–77 MB/s、`engine_relay_throughput_loopback` 引擎+WG ≈ 5 MB/s）均 `#[ignore]` 按需跑（`--ignored`），避免与正确性测试并行抢 CPU 致偶发超时。
3. **对称 NAT 端口预测 + 生日喷洒**（双校园网 v4，参考 EasyTier）：大特性，独立分支，先做端口预测的最小验证。
4. **借道第三方节点中继**（peer-relay，数据面不过小服务器）：大特性，依赖 3 的连通性，独立分支。
5. **发布形态**：预编译二进制 + 安装器，开箱即用、安装时检测下载依赖、下载走国内镜像。独立分支。
6. **合 main**：`gh` 未登录，需用户 `gh auth login` 后建 PR（AGENTS.md：不直推 main）。当前 TCP 修复在 `feature/m1-scaffold`，待 PR。

> 接手建议：2–5 都是独立大特性，先与用户确认本轮要做哪一项再动手；默认优先 2（兜底可用性），其余按需。

## 11. 约定与偏好（来自 AGENTS.md + 长期记忆）

- 工程规范见 `D:\repo-REPO\Ara\AGENTS.md`：feature 分支 + PR 合 main、小单元提交、动词+目标 commit message。
- UI/设计酌情参考 github.com/emilkowalski/skills；工程细节用 github.com/mattpocock/skills；回复风格参照 github.com/ayghri/i-have-adhd（简洁直接）。
- 下载尽量走国内镜像。
- 开箱即用：最终交付不依赖用户装 Rust，依赖安装时检测下载。
- 不改用户网络环境（DNS/路由），仅指定端口通信。
- 文件保护：删除走 host_safe_delete（回收站），改用户文件先备份（git 仓库内除外）。
```