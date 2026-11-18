# Ara 项目约定

## 三方同步与全量测试（用户 2026-11-18 明确要求）
- 每次代码改动必须同步到三方：本机 `D:\repo-REPO\Ara`、WSL 源码副本 `~/ara`（UNC `\\wsl$\Ubuntu\root\ara`）、云服务器 `root@128.14.237.95:/root/ara-src`。
- 同步后做全量测试：`cargo test --workspace` + `cargo clippy --workspace --all-targets`；涉及真机路径还要按交付报告 §6 部署并 ping/iperf 复测。
- 部署顺序：先 `pkill -x ara` 再替换二进制，重启客户端；**不动 `ara-server`**（需用户确认）；记录部署的提交号 + 二进制 md5，避免把旧版表现误判为新代码结果。
- 环境事实：`wsl.exe` 被沙箱程序黑名单拦截 → WSL 内无法执行编译/测试，但源码可通过 UNC 同步；VPS 已装 gcc + rustc/cargo 1.100.0，可就地 `cargo test` / `cargo build --release`（`CARGO_BUILD_JOBS=1` 防 764MB 内存 OOM）。
- 本机 Windows 无法交叉编译 musl（`ring 0.16.20` 需要 `x86_64-linux-musl-gcc`），Docker daemon 未运行；VPS 编译 gnu 动态二进制即可本机部署。

## M2 完整交付（2026-11-18 12:25）
- `gh` 已登录 srf-27（keyring token，https remote）。按用户本轮指令**直推 main**（明确覆盖 AGENTS.md 的 PR 规范）：远端 main 当时已有 PR #1 的空合并节点 `7991561`，先 `git merge --no-edit origin/main` 再 push，无冲突、未丢历史；远端 main = `b3f6094`。
- 全量测试：本机 + VPS 各 29 默认测试 + 6 ignored、clippy 零警告；WSL 只同步源码（`wsl.exe` 黑名单，执行受阻）。
- 四路径吞吐诊断在 `ara/tests/throughput.rs`（4 个 ignored），数值见交付报告 §13.2；裸 ws 诊断改为固定窗口以免弱机超时。
- Windows 会话非管理员 → 起不了 TUN 客户端，真机跨机吞吐未复测，沿用报告 §4.3 历史值（TCP 直连 270 Mbps / WAN relay ≈3 Mbps）。
- VPS 部署的二进制对应 `edb3d7c` 产品代码；之后提交只有测试与文档，产品代码未变。
