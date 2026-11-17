# Ara 交付报告（2026-11-18）

> 用途：另一个 agent 仅凭本报告 + `AGENTS.md` 即可接手继续开发。历史细节与排查过程见 `docs/handoff-tcp-direct.md`（§5 两个死锁 bug、§7 环境陷阱、§8 部署拓扑）。本报告只写当前事实与下一步。

## 1. 项目一句话

P2P 虚拟局域网（`10.99.0.0/24`，L3，任意协议互访）。Rust + tokio + boringtun（用户态 WireGuard）+ tun crate（wintun / /dev/net/tun）。路径优先级：**UDP 直连/打洞（IPv6 优先）→ TCP 直连 → 服务器 ws 中继兜底**。小服务器只做控制面（信令 + STUN + 虚拟 IP 分配）+ 中继兜底。MTU 1280。

## 2. 当前状态（事实快照）

- 仓库 `D:\repo-REPO\Ara`，远程 `github.com/srf-27/Ara.git`，**默认分支 main**（AGENTS.md 写 master，以仓库实际为准）。
- 工作分支 `feature/m1-scaffold`，**领先 origin 6 个提交，未推送**（`gh` 未登录，PR 被阻，需用户 `gh auth login`）：
  ```
  ad18c5c docs: record relay-load engine deadlock fix and real-machine re-measurement
  e5c42f7 fix(ara): keep engine loop non-blocking under relay/TCP backpressure   ← 本轮核心
  0d8cb68 docs: record relay bottleneck localization (WAN TCP-over-TCP, not code) ← 结论已被 e5c42f7 修正
  aeb287e test(ara): add relay throughput benchmarks to localize bottleneck
  850f1d9 docs: mark TCP deadlock fixed, correct stale facts, scope M2
  c70f624 fix(ara): resolve direct-TCP handshake deadlock, enable TCP_DIRECT
  ```
- 工作树干净。`cargo test --workspace` 全绿：**ara lib 13 + ara-server lib 5 + 集成 4 默认跑**（另 2 个吞吐诊断 `#[ignore]`，`cargo test --test loopback -- --ignored` 按需）。`cargo clippy --workspace --all-targets` 无警告。
- M1 全部能力已实现并真机验证（注册/稳定 IP/STUN/打洞/TUN/TCP 直连/relay/保活重连）。M2 已完成「relay 兜底可用性」（本轮，见 §4）。

## 3. 代码地图（行号为当前实际值）

workspace 两 crate：`ara`（客户端 CLI + 共享协议）、`ara-server`（信令/STUN/中继）。

### ara/src/engine.rs（数据面核心，单任务事件循环）
| 位置 | 内容 |
|---|---|
| L20–26 | `BUF=2048, TCP_DIRECT=true, TICK=250ms, PUNCH_INTERVAL=2s, STUN_TIMEOUT=3s` |
| L40 | `Command`：AddPeer/SetPeers/RemovePeer/Stun/RelayIn/TcpUp/TcpAccepted/TcpIn/TcpClosed |
| L89 | `send_tcp()`：**非阻塞** TCP 发送助手。try_send；Closed→清 tcp_tx、tcp_proven=false、use_relay=true；Full→跳过该包保留 sender |
| L103 | `apply()`：出站路由。tcp_tx/relay_out/ip_in **全部 try_send**（引擎循环永不阻塞，见 §4.2 不变量） |
| L143 | `punch()`：发 handshake initiation（TCP probe + relay + UDP 候选） |
| L160 | `Peer`：含 `tcp_proven`（收到过 TCP 上的合法 WG 包才置真）、`use_relay`、`last_direct`、`tcp_tx` |
| L222 | `Engine::run`：select! 四路（UDP recv / ip_out / cmd / tick 250ms） |
| L302 | `TcpClosed` handler：清 tcp_writers + 自愈已关闭的 peer.tcp_tx |
| L370 | `attach_tcp()`：拆 reader/writer 任务。**writer write_all 5s 超时、reader 首帧 60s 空闲超时**（黑洞连接回收）。outbound→peer.tcp_tx；inbound→tcp_writers[conn] |
| L445 | `ingest_tcp()`：成功 decap 即 `tcp_proven=true, use_relay=false`（**静默置位，无 info 日志**，除非收养 inbound writer） |
| L513 | `ingest()`：UDP/relay 入站；UDP 来源刷新 last_direct 并关 relay |
| L558 | `on_tick()`：relay 激活（created>8s && last_direct>10s && !tcp_proven）；TCP 连接尝试（tcp_tx 为空 && last_direct>5s && last_tcp_try>5s，3s connect 超时）；update_timers |

### ara/src/client.rs（控制面）
- L21 `PUBLIC_STUN`（stun.qq.com / stun.miwifi.com，服务器 TCP-only 时回退）；L24 身份密钥落盘（Windows `%APPDATA%/ara/identity.key`，Linux `~/.config/ara/identity.key`）；L65 `run`（永久重连，1–30s 指数退避，FatalAuth 即退）；L116 `session`（注册→Welcome→STUN→Candidates→心跳 15s/候选刷新 30s）；L249 `tcp_candidates`（本机接口 + STUN 反射 IP × tcp_port）。
- TCP 兜底监听 `0.0.0.0:tcp_port`，accept→`Command::TcpAccepted`。

### ara-server/src/lib.rs（未改动，无需重新部署）
- `State`：按网络名+密钥注册，虚拟 IP 按公钥找回、从 .1 顺序分配（254 上限），45s 离线清扫，分配表 JSON 落盘。
- `handle_conn`：ws Text 信令；**ws Binary 中继帧 = [32B 目的公钥][WG 包]**，base64 查 registry → unbounded mpsc → 目的连接 sink.send。accept 即 set_nodelay。

### 协议与帧格式
- 信令：ws Text JSON（Register/Welcome/Candidates/Peers/Heartbeat/Error），`ara/src/proto.rs`。
- 中继：ws Binary `[32B key][WG]`（服务器剥前缀转发）。
- TCP 直连传输：**2B 大端长度 + WG 包体**。
- 通道容量（client.rs）：cmd 256 / relay 256 / ip 256。**有界通道 + 单循环 ⇒ 一切出站发送必须 try_send**（§4.2）。

## 4. 本轮交付：两个死锁 bug 的修复（都已验证）

### 4.1 TCP 握手死锁（c70f624，前一轮）
鸡生蛋：`tcp_proven` 要求先收到 TCP 上的 WG 包，但旧 apply 在未证明时从不走 TCP → initiation 到不了对端。修复：未证明时 TCP probe 克隆叠加 relay/UDP 兜底；boringtun 每 5s（REKEY_TIMEOUT）重发 initiation，5s 内即可 prove。测试 `two_clients_exchange_ip_packets_over_tcp` 30s 超时→5s 通过。

### 4.2 relay 负载死锁（e5c42f7，本轮核心）
**症状**：WSL↔VPS relay 静止 ping 正常，iperf 负载后双向 100% 丢包；一端引擎零日志（循环卡死），另一端刷 REKEY_TIMEOUT/CONNECTION_EXPIRED。
**根因**：单循环对有界通道阻塞 `.await`——① tcp_tx writer 的 `write_all` 在黑洞 TCP（NAT SYN 代理：connect 成功、payload 丢）上永久 pending → 通道满 → `tx.send().await` 冻住循环；② relay_out 出口饱和时 `send().await` 冻住循环 → 处理不了入站 ACK。
**修复**：tcp_tx/relay_out/ip_in 全 try_send（满则丢包，WG/上层重传）；writer 5s 写超时 + reader 60s 空闲超时回收黑洞连接。
**确立的不变量（后续改动必须遵守）**：**引擎单循环内禁止对任何传输通道/可能停滞的 IO 用阻塞 `.await`；出站一律 try_send + 丢包回退。** 回归测试 `relay_backpressure_keeps_engine_responsive`（暂停排空 relay_rx 灌满 relay_out，断言引擎仍响应 Stun 且恢复投递；修复前必失败）。
**注意**：`0d8cb68` 文档里「relay 代码健康、纯 TCP-over-TCP meltdown」的结论**已被推翻**——真机慢/死的主因就是这个死锁。修复后 TCP-over-TCP 仍限制吞吐（~WAN 上限）但不再死锁，cwnd 健康收敛。

### 4.3 实测数据（验收基线）
| 场景 | 结果 |
|---|---|
| Win↔WSL TCP 直连（前一轮） | 270 Mbps（33.7 MB/s），2ms |
| WSL↔VPS relay，TCP iperf 15s | sender 5.5MB/3.08Mbps，receiver 4.5MB/2.33Mbps，**0 重传**，cwnd 106KB→914KB |
| WSL↔VPS relay，UDP iperf 10s@1M | **0/1019 丢包**，jitter 0.198ms |
| relay 负载前后 ping | 0% 丢包，RTT ~320ms（校园网→境外 VPS） |
| loopback 裸 ws 中继（ignored 诊断） | ~50–77 MB/s |
| loopback 引擎+WG relay（ignored 诊断） | ~5 MB/s |

relay ~3Mbps 是 WAN 带宽/RTT 上限，**非代码瓶颈**；relay 仅兜底，不要再为它改架构（批处理/多连接无收益）。

## 5. 运行中的测试环境（当前事实）

| 角色 | VIP | 状态 |
|---|---|---|
| VPS 128.14.237.95（CentOS Stream 9，1C764M，云 NAT） | .1 | ara-server（`--no-stun --state /root/ara-state.json`，TCP+UDP 23947，日志 /root/ara-server.log）+ ara client（修复版 musl，`--port 23947 --tcp-port 23948`，日志 /root/ara-client.log）+ `iperf3 -s -D`（/root/iperf3.log）均在跑 |
| WSL2 Ubuntu（root） | .3 | 修复版 debug bin `~/ara/target/debug/ara`，日志 /root/clientB.log，在跑 |
| Windows 宿主 | .2 | **未运行** |

- 网络名 `demo` 密钥 `test`（测试值）。ssh：`ssh -i "C:\Users\Lenovo\.ssh\id_rsa" root@128.14.237.95`。
- **VPS 黑洞**：TCP 23948 payload 被 provider SYN 代理黑洞（connect 成功、数据不到）；这是 §4.2 的触发条件，**不要去查防火墙**。UDP 23947 正常。
- **校园网封 UDP 出口**（移动 NAT，出口 IP 会变）→ WSL 只能走 relay；WSL STUN 反射当前 117.147.100.235:7779。
- WSL 已改（测试 VM，非用户主环境）：/etc/resolv.conf=223.5.5.5/119.29.29.29（wsl.conf generateResolvConf=false）、apt 源→aliyun（备份 ubuntu.sources.bak）、装了 iperf3。
- 清理约定：本地删除走回收站；WSL/VPS 移 `/root/.ara-test-trash`。停测试：VPS/WSL `pkill -x ara`（server 进程名 ara-server，勿误杀）。

## 6. 常用命令（已验证可靠）

```sh
# Windows 测试/构建（客户端运行时 exe 被锁，先按 Id 强杀：Get-Process ara | % { Stop-Process -Id $_.Id -Force }，需管理员）
cargo test --workspace && cargo clippy --workspace --all-targets
cargo test --test loopback -- --ignored          # 吞吐诊断（勿与默认套件并行，会抢 CPU 偶发超时）

# WSL 构建（务必在 ~/ara，/mnt/d 上编译极慢；非登录 shell 先 source ~/.cargo/env）
cp /mnt/d/repo-REPO/Ara/ara/src/engine.rs ~/ara/ara/src/   # 同步改动
cd ~/ara && cargo build -p ara                             # WSL debug
cargo build --release -p ara --target x86_64-unknown-linux-musl
cp target/x86_64-unknown-linux-musl/release/ara /mnt/d/repo-REPO/Ara/target/ara-linux

# 部署 VPS（用 Windows 的 ssh/scp，WSL 的 known_hosts 没有 VPS 主机键）
ssh -i /c/Users/Lenovo/.ssh/id_rsa root@128.14.237.95 'pkill -x ara; sleep 1'   # 先杀，否则 scp 覆盖运行中二进制失败
scp -i /c/Users/Lenovo/.ssh/id_rsa /d/repo-REPO/Ara/target/ara-linux root@128.14.237.95:/usr/local/bin/ara
ssh ... 'setsid nohup /usr/local/bin/ara up --network demo --secret test --server 128.14.237.95:23947 --port 23947 --tcp-port 23948 </dev/null >/root/ara-client.log 2>&1 & echo started'
# ssh 通道可能挂住但命令已执行：开新连接验证，勿重复执行写操作

# WSL 重启客户端（& 后必须 sleep 保持会话，否则 WSL 实例立即回收后台进程）
wsl -d Ubuntu -u root -- bash -c 'pkill -x ara; sleep 1; setsid nohup ~/ara/target/debug/ara up --network demo --secret test --server 128.14.237.95:23947 </dev/null >/root/clientB.log 2>&1 & sleep 10; tail /root/clientB.log'

# relay 复测（WSL 内）
ping 10.99.0.1; iperf3 -c 10.99.0.1 -t 15; iperf3 -c 10.99.0.1 -u -b 1M -t 10
```

## 7. 环境陷阱（会浪费大量时间，先看）

1. Windows exe 被运行中客户端锁住 → 链接 os error 5；管理员按 Id 强杀。
2. 提权脚本（RunAs）里 rustup PATH 不同 → 显式 `$env:RUSTUP_TOOLCHAIN="stable"` 或全路径 cargo。
3. clippy 与 rustc 版本不一致报 E0514 → 删工具链目录陈旧 clippy 残留再 `rustup component add clippy`（详见 handoff §7.8）。
4. ssh 里 nohup 后台命令可能挂住通道但**已执行** → 新连接查状态，不确定时先查再写。
5. 管道后 `$?` 是最后一个命令（grep）的退出码 → 用 md5 等实际状态验证传输。
6. tokio current-thread 测试运行时会饿死任务 → 重负载测试用 `flavor="multi_thread"`；重负载测试之间会互相抢 CPU，吞吐基准一律 `#[ignore]`。
7. VPS TCP 23948 黑洞是 provider 行为（§5），不是防火墙。

## 8. 已知限制（不是 bug，勿当 bug 修）

- 双对称 NAT 且无 IPv6 → 直连可能打不通，自动退 relay（M2 项 1 解决）。
- 仅 L3，无二层广播发现；每网络 254 peer；信令明文 ws（TLS 未做）；secret 明文存配置。
- Windows 客户端需管理员（wintun），Linux 需 root。
- relay 吞吐 ≈ WAN 上限（TCP-over-TCP + 高 RTT），设计如此。

## 9. 剩余工作（M2，按优先级）

每项独立分支、最小可验证步骤、PR 合 main（AGENTS.md）。**动手前先与用户确认做哪一项。**

1. **对称 NAT 端口预测 + 生日喷洒**（双校园网 v4 场景，参考 EasyTier）：大特性。最小第一步：在现有 candidates/打洞机制上加「对对称 NAT 对端的端口增量预测 + 批量候选喷洒」，用两个对称 NAT 真机验证打洞成功率。依赖：STUN 已能判别 NAT 类型（当前未实现类型判别，需先加）。
2. **peer-relay（借道第三方节点中继）**：数据面不过小服务器。依赖项 1 的连通性。大特性，独立分支。
3. **发布形态**：预编译二进制 + 安装器，开箱即用（不依赖用户装 Rust），依赖安装时检测下载、走国内镜像。Windows 已有 wintun.dll 内嵌首启释放逻辑。
4. **合 main**：用户 `gh auth login` 后，把 `feature/m1-scaffold`（6 个未推送提交）建 PR 合 main。PR 说明需含：两个死锁修复的根因/验证数据（§4）、兼容性风险（无协议变更，纯客户端行为修复）。
5. 可选加固（小项）：信令 TLS（wss）；`ingest_tcp` 置 tcp_proven 时加一条 info 日志（当前静默，排查不便，本轮已吃亏）。

## 10. 必须遵守的约定（AGENTS.md 摘要）

- 懒惰资深模式：最小完整改动，先定位瓶颈再动架构，不加没人要的功能/抽象；新非平凡逻辑必须带一个小测试。
- git：feature/bugfix 分支，小单元提交 `feat|fix|docs|test|chore(scope): 动词+目标`；提交前看 `git diff --cached` + `status`；**不直推 main，PR 合并**；不提交密钥/.env/构建产物；不改 .gitignore。
- 不改用户 DNS/路由（WSL 测试 VM 的改动已在 §5 登记）；下载走国内镜像；删除走回收站/测试 trash 目录。
- 回复末尾一两行说明跳过/未查项与风险。