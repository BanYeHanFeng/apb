# AGENTS.md

本仓库是 **apb（Agent Proxy Bridge）**：一个静态单文件同时承担 `serve` / `agent` / 控制器三种角色。
项目统一使用 `apb`，不再绑定“手机”这一种节点。

## 先读

1. [`README.md`](README.md) —— 项目简介、快速开始和文档索引。
2. [`docs/使用手册.md`](docs/使用手册.md) —— 架构、服务端 / Agent / 控制器、命令与文件传输契约。
3. [`docs/构建与测试.md`](docs/构建与测试.md) —— 本地构建、静态交叉编译、构建工作流、测试与发布。
4. [`docs/协议.md`](docs/协议.md) —— APB0 线协议（apb 0.1.0）、Payload、退出码。
5. [`docs/安全.md`](docs/安全.md) —— 信任边界与密钥管理。
6. [`docs/故障排查.md`](docs/故障排查.md) —— 常见故障。
7. [`skills/apb/SKILL.md`](skills/apb/SKILL.md) —— AI 可执行速用手册。

## 核心约束

- 节点端不装 sshd / openssh / 任何系统包；一个静态二进制 `apb` 即可运行。
- 连接只需要 **服务端地址 + 32 字节密钥**。
- 配置只读环境变量 / 命令行参数：`APB_SERVER`、`APB_BIND`、`APB_KEY`、`APB_NAME` 等。
- 不创建配置文件、known_hosts、私钥文件；`push` / `pull` 只写用户显式指定的目标。
- GitHub Actions 从 `secrets.APB_SERVER` / `secrets.APB_KEY` 读取；公开仓库也能用。
- 红线：服务端只路由；命令只会在 agent 的 shell 中执行。

## 开发约定

- 构建 / 测试可放在临时 VPS 或 GitHub Actions，也可本地原生执行：
  ```bash
  cargo fmt --check
  cargo test --locked --all-targets
  cargo zigbuild --release --locked --target x86_64-unknown-linux-musl
  cargo zigbuild --release --locked --target aarch64-unknown-linux-musl
  ```
- 回归测试：`tests/e2e.rs`（`cargo test` 会自动在 loopback 拉起 server + agent）。
- 发布正式版：把提交标题写为 `V0.1.1正式版` 并推送 `main`（自动创建 `v0.1.1` tag），或直接推送 `v0.1.1` tag；`构建与发布` 工作流会发布 latest 正式版。
- 新能力优先只改 `src/` 与 `docs/`；保持“无 sshd、无配置文件”的约束。

## 常用命令

```bash
apb keygen
apb serve --bind 0.0.0.0:30020 --key "$APB_KEY"
APB_SERVER=IP:30020 APB_KEY="$APB_KEY" APB_NAME=node-a apb agent
apb status --server IP:30020 --key "$APB_KEY" --json
apb exec   --server IP:30020 --key "$APB_KEY" --name node-a --json -- 'uname -a'
apb push   --server IP:30020 --key "$APB_KEY" --name node-a ./f '~/f'
apb pull   --server IP:30020 --key "$APB_KEY" --name node-a '~/out' ./out
```
