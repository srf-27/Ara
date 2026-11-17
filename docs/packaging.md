# 发布形态（最小可验证版）

产物在 `target/dist/`（已被 `.gitignore` 忽略，不进仓库）。Windows 客户端内嵌官方 wintun.dll，首启自动释放到 exe 同目录；包里另附一份供离线排错。

## 构建

```sh
bash scripts/build-release.sh                 # 当前平台：ara + ara-server
bash scripts/build-release.sh --musl          # WSL 里编 VPS 用的静态 ara
```

产物命名：`ara-<target>.tar.gz`，解压后固定为 `ara/` 目录（含二进制、`ara.toml.example`、README）。

## 安装（离线优先，不依赖装 Rust）

```sh
# Linux / WSL：把 dist 目录或 tar 包传给脚本
ARA_PREFIX=~/ara-bin bash scripts/install.sh target/dist/ara
ARA_PREFIX=~/ara-bin bash scripts/install.sh target/dist/ara-x86_64-unknown-linux-musl.tar.gz

# Windows PowerShell：装到用户目录，无需管理员；运行 ara up 时才需要管理员
powershell -File scripts/install.ps1 -Source target\dist\ara
```

`install.sh` 无参数时从 `${ARA_MIRROR}/<ARA_ASSET>` 下载，默认镜像为 ghfast.top 代理 GitHub Releases，`install.ps1` 同理（`-Mirror`）。镜像与 asset 名都可用环境变量/参数覆盖；**当前仓库尚未发布 Release assets**，联网安装需先把产物托管到可达地址并设 `ARA_MIRROR`。

## 验证

```sh
ara --help                                    # clap 帮助，证明二进制可运行
bash -n scripts/build-release.sh scripts/install.sh
ARA_PREFIX=/tmp/ara-test bash scripts/install.sh target/dist/ara && /tmp/ara-test/ara --help
```

安装脚本不写 DNS、不改路由、不动防火墙；只复制二进制和配置样例。配置 `ara.toml` 后以管理员/root 运行 `ara up --config ara.toml`。

## 已知限制

- 预编译二进制未发布到线上；联网安装路径只有下载+解压逻辑，未做外网连通验证。
- 未做服务化（systemd / Windows 服务）与自动升级，需要时再加。
