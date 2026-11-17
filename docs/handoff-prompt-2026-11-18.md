# 接手提示词（复制下面整段给下一个 agent）

---

你在 `D:\repo-REPO\Ara` 接手 Ara（Rust P2P 虚拟局域网）的 M2 开发。上一轮已交付并真机验证，你的职责是继续，不是重做。

**开工前按顺序读：**
1. `AGENTS.md`（工程规范，强制遵守：最小完整改动、小单元提交、PR 合 main、不直推、回复末尾交代跳过项与风险）
2. `docs/delivery-report-2026-11-18.md`（当前事实快照：代码地图、已修 bug、实测基线、运行中的测试环境、命令、陷阱、M2 剩余项）
3. `docs/handoff-tcp-direct.md` §5/§7（两个死锁 bug 的根因细节与环境陷阱，排查时再精读）

**然后核实状态（只读，勿先写）：**
- `git status` + `git log --oneline -8`：应在 `feature/m1-scaffold`，领先 origin 7 个提交，工作树干净。
- `cargo test --workspace` + `cargo clippy --workspace --all-targets`：应全绿（22 个默认测试 + 2 个 ignored 吞吐诊断）。
- 与报告不符时先查明原因再动手，勿盲目覆盖。

**必须遵守的技术不变量：**
- 引擎是单事件循环（`ara/src/engine.rs` `Engine::run`）：循环内对传输通道（tcp_tx/relay_out/ip_in）一律 `try_send`，禁止阻塞 `.await`，满则丢包回退。这是上一轮真机死锁的根因修复，回归测试 `relay_backpressure_keeps_engine_responsive` 守护，勿破坏。
- 路径优先级 UDP 直连 → TCP 直连 → relay 兜底；`tcp_proven` 门控与 relay 激活条件见报告 §3，改动前先读懂。
- relay ~3Mbps 是 WAN 上限不是代码瓶颈，勿为 relay 吞吐改架构。
- VPS TCP 23948 payload 黑洞是 provider SYN 代理行为，勿查防火墙；校园网封 UDP 出口是既定测试场景。

**任务选择：** M2 剩余项见报告 §9（对称 NAT 端口预测喷洒 / peer-relay / 发布形态 / 合 main / 小加固）。都是独立特性，**先问用户本轮做哪一项，确认后再动手**；每项独立分支、先做最小可验证步骤。若用户要求合 main：需先 `gh auth login`（当前未登录），PR 说明含两个死锁修复的根因与验证数据。

**完成标准：** 新非平凡逻辑带回归测试；`cargo test --workspace` + clippy 全绿；涉及真机路径的改动按报告 §6 命令部署 WSL/VPS 复测（先杀旧进程再 scp，ssh 挂住时开新连接验证、勿重复写操作）；小单元提交（`feat|fix|docs|test(scope): 动词+目标`），提交前看 `git diff --cached`。

**禁止：** 直推 main；提交密钥/构建产物；改 .gitignore；改用户 DNS/路由；未经确认杀 ara-server 或删测试环境文件（删除走回收站 / `/root/.ara-test-trash`）。

---