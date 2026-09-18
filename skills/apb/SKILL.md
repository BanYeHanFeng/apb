---
name: apb
description: 用静态单文件 apb 把 Termux / GitHub Actions runner / 任意 Linux 节点接成远程执行 agent；只需要服务端地址、端口和 32 字节密钥。默认链路：apb agent 出站连接 apb serve，apb exec/push/pull/status 从本机控制；不装 sshd、不落配置文件。
whenToUse: 用户说“在手机上跑/装/看一下…、取/传手机文件、把手机接进来”，或“在临时 CI/干净机器上跑命令、要 root、把 Actions runner 当临时 VPS、取回结果”，或需要排查 apb agent/serve 链路时。
metadata:
  repo: /root/github/apb
  language: Rust
  cli: apb（单二进制多角色：serve / agent / exec / push / pull / status / doctor / keygen）
  docs: README.md · docs/使用手册.md · docs/构建与测试.md · docs/协议.md · docs/安全.md · docs/故障排查.md
---

# apb（Agent Proxy Bridge）

一个静态单文件 `apb` 同时承担三种角色：

| 角色 | 命令 | 运行位置 | 说明 |
|---|---|---|---|
| 服务端 | `apb serve` | 有公网 IP 的机器 | 认证、注册、按名路由；不执行用户命令 |
| Agent | `apb agent` | Termux / Android / GitHub Actions runner / Linux 节点 | 出站连接服务端，断线自动重连 |
| 控制器 | `apb exec/push/pull/status/doctor` | 本机 / AI 运行端 | 经服务端把命令和文件送到指定 agent |

核心约束：

- 节点端不依赖 sshd / openssh / `pkg install`，不需要配置文件。
- 只需要 `服务端地址 + 32 字节密钥`。
- 配置只读环境变量或命令行参数：`APB_SERVER` / `APB_BIND` / `APB_KEY` / `APB_NAME`。
- Actions 里从 `secrets.APB_SERVER` / `secrets.APB_KEY` 读取；公开仓库也可用，runner 跑完即焚。
- 构建在临时 VPS / Actions 上进行（`cargo zigbuild` 交叉出静态 musl 二进制），不在运行端装 Rust。

## 速用

```bash
# —— 在临时 VPS / GitHub Actions（x86_64）上构建 ——
cargo test --locked --all-targets
cargo zigbuild --release --locked --target x86_64-unknown-linux-musl

# —— Termux 用 aarch64 静态产物 ——
cargo zigbuild --release --locked --target aarch64-unknown-linux-musl

# —— 服务端（生成/保存密钥，不创建配置文件）——
export APB_KEY="$(./apb keygen)"
./apb serve --bind 0.0.0.0:30020 --key "$APB_KEY"

# —— 手机 / runner 客户端 ——
APB_SERVER='IP:30020' APB_KEY="$APB_KEY" APB_NAME='phone-a' ./apb agent

# —— 节点一键接入（脚本自动下载二进制并交互询问参数）——
# bash <(curl -fsSL https://raw.githubusercontent.com/BanYeHanFeng/apb/main/runApb.sh)
# 交互运行时也会询问 1 正式版 / 2 预发布；非交互可追加 --pre（或设置 APB_CHANNEL=prerelease）

# —— 本机控制 ——
./apb status --server IP:30020 --key "$APB_KEY" --json
./apb exec   --server IP:30020 --key "$APB_KEY" --name phone-a --json -- 'uname -a'
./apb push   --server IP:30020 --key "$APB_KEY" --name phone-a ./file '~/file'
./apb pull   --server IP:30020 --key "$APB_KEY" --name phone-a '~/logs' ./logs
./apb doctor --server IP:30020 --key "$APB_KEY" --json
```

单个 agent 在线时可以省略 `--name`。

## 必守规则

1. **先 `status`**：`apb status --json` 看到 agent 才执行后续命令。
2. 多个 agent 时必须指定 `--name`；默认名是 `USER@HOSTNAME`，手机之间常会重名，建议启动 agent 时就设置 `APB_NAME`。
3. `exec` 普通模式退出码 = 远端 rc；`--json` 模式 rc 在 JSON 的 `rc` 字段。
4. 真超时 JSON：`timed_out:true`、`ended_reason:"timeout"`，退出码 5。
5. `--cwd` 不存在返回 `bad_cwd`，退出码 125，不会静默回退到 `$HOME`。
6. `--` 之后的参数按顺序拼接成一条命令交给 agent 的 shell：
   `apb exec ... -- 'cmd1; cmd2'`。
7. push/pull 的远端路径不做本机 shell 展开；`~` 加引号交给 agent 解析。
8. `doctor` 只检查到服务端的 Noise 握手，不代表 agent 在线。
9. 不打印 `APB_KEY`；不把 `APB_KEY` 写进文件、命令参数或 workflow inputs。
10. agent 连不上时看它打印的 `stage=`：`resolve` / `connect` / `handshake` / `hello` / `hello_reply`，后接 `after Nms`。
    `os error 11`（EAGAIN）是本机 socket 失败或超时，不是密钥错误（密钥不符表现为
    `stage=handshake ... failed to fill whole buffer`，同时服务端日志出现 `noise read: decrypt error`）。
    重试间隔约 1 秒＝每次立刻失败（本机 / 链路拒绝），十几秒＝在等超时；手机端优先排查
    VPN / 代理（sing-box、Clash、v2rayNG）、省电与后台数据限制、私有 DNS，并换网络对照。

## 输出契约

`apb exec --json`：

```json
{
  "ok": true,
  "rc": 0,
  "duration_ms": 59,
  "timeout_s": 300,
  "cwd": "",
  "command": "echo hi",
  "stdout": "hi\n",
  "stderr": "",
  "stdout_bytes": 3,
  "stderr_bytes": 0,
  "stdout_truncated": 0,
  "stderr_truncated": 0,
  "timed_out": false,
  "ended_reason": "ok"
}
```

- `--b64`（配合 `--json`）得到 `stdout_b64` / `stderr_b64`。
- `--max-output N` 只限制 JSON 中保留的字节数；远端命令仍会被完整 drain 到结束。
- `push` / `pull --json` 以 `ok`、`remote_path` / `local_path`、`bytes` 为准。

## 退出码

| 码 | 含义 |
|---|---|
| 0 | 成功 |
| N | exec 普通模式的远端命令 rc（信号为 128+signal） |
| 1 | 本地错误 / 文件传输本地失败 / 远端执行失败 |
| 2 | 缺少或非法 APB_KEY、缺少服务端地址 |
| 3 | 连接失败 / 协议错误 / 服务端错误 |
| 5 | 超时 |
| 6 | 文件传输失败 |
| 64 | 用法错误 |
| 125 | exec `--cwd` 目录不存在 |

## 常见动作

```bash
# 看有哪些 agent
apb status --server IP:30020 --key "$APB_KEY"

# 传整个目录
apb push --server IP:30020 --key "$APB_KEY" --name phone-a ./site '~/site'

# 取回结果
apb pull --server IP:30020 --key "$APB_KEY" --name phone-a '~/out.log' ./out

# 用 GitHub Actions 临时节点（在仓库里先配置 secrets）
#   手动触发 .github/workflows/apb-agent.yml
#   出现后： apb status / apb exec --name github-... ...
```

详细使用方式见 `docs/使用手册.md`；线协议见 `docs/协议.md`；故障排查见 `docs/故障排查.md`。
