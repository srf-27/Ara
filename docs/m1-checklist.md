# M1 真机验收清单

自动化测试已覆盖：协议编解码、STUN、IP 分配、候选过滤、server+2 逻辑客户端 loopback 加密互通（`cargo test --workspace`）。
本清单验证**真实网卡 + 真实 NAT** 下的闭环，需两台机器（至少一台校园网）。

## 准备

1. 小服务器（公网，1~3M 带宽即可）运行控制面：
   ```sh
   ./ara-server --port 23947
   ```
   放行 TCP 23947（信令）+ UDP 23947（STUN）。
2. 两台客户端机器分别构建 `ara`（Windows 需管理员，Linux 需 root）。
3. 同一台机器上同时跑 server 和客户端时，客户端须加 `--port 0`（或别的端口），避免与服务器 UDP 23947 冲突。单机自测可用 WSL2/虚拟机当第二台设备（自带 NAT，是真实打洞场景）。

## 验收项

### 1. 入网与虚拟 IP
- [ ] 两端用相同 `network` / `secret` 启动：
  ```sh
  ara up --network demo --secret <密钥> --server <服务器IP>:23947
  ```
- [ ] 日志出现 `assigned virtual IP 10.99.0.x`，两端 IP 不同。
- [ ] 日志出现 `STUN reflexive address ...`（公网映射）。
- [ ] 虚拟网卡出现（Windows 名为 `ara`；Linux `ip addr` 可见），MTU 1280。

### 2. 直连互通
- [ ] 互 `ping 10.99.0.x` 通。
- [ ] 日志/抓包确认走直连候选（IPv6 或打洞后的公网映射），而非服务器。

### 3. 吞吐（iperf3）
- [ ] 一端 `iperf3 -s`，另一端 `iperf3 -c 10.99.0.x`。
- [ ] TCP 吞吐接近双方物理带宽下限（直连路径），明显高于 1~3M 服务器带宽。

### 4. 大文件传输
- [ ] 传一个 ≥100MB 文件（Windows SMB 共享或 `python -m http.server` 均可）。
- [ ] 两端哈希一致（`certutil -hashfile` / `sha256sum`）。

### 5. 保活与重连
- [ ] 静置 >45s 后仍能互通（25s keepalive 维持 NAT 映射）。
- [ ] 临时断开服务器再恢复，客户端自动重连并恢复互通。

## 已知限制（M1）
- 双对称 NAT 且无 IPv6 时直连可能打不通，自动退服务器中继兜底（端口预测喷洒 / 借道第三方中继在 M2+）。
- 服务器重启后按公钥找回原虚拟 IP（密钥与分配表已落盘）；短暂离线期间互通中断，重连后恢复。
- 仅 L3，无二层广播（"网络邻居"自动发现不可用，直接敲 IP 正常）。