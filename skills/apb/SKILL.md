---
name: apb
description: 远程控制任意 Linux 节点
whenToUse: 用户说“在手机上跑/装/看一下…、取/传手机文件、把手机接进来”，或“在临时 CI / 干净机器 / GitHub Actions runner 上跑命令、要 root、把 runner 当临时 VPS、取回结果”，或需要排查 apb agent/serve 链路时。
metadata:
  repo: /root/github/apb
  language: Rust
  binary: apb（PATH）/ target/release/apb（仓库构建）/ runApb.sh（节点一键下载）
  cli: apb（单二进制：serve / agent / status / exec / push / pull / doctor / keygen）
  docs: README.md · docs/使用手册.md · docs/构建与测试.md · docs/协议.md · docs/安全.md · docs/故障排查.md
---

# apb（Agent Proxy Bridge）AI 速用手册

一个静态单文件 `apb` 同时承担三种角色：

| 角色 | 命令 | 运行位置 | 说明 |
|---|---|---|---|
| 服务端 | `apb serve` | 有公网 IP 的机器 | 认证、注册、按名路由；不执行用户命令 |
| Agent | `apb agent` | Termux / Android / Actions runner / Linux | 主动出站连接服务端，断线自动重连 |
| 控制器 | `apb status/exec/push/pull/doctor` | 本机 / AI 运行端 | 经服务端把命令和文件送到指定 agent |

核心约束：

- 只需要服务端地址 + 32 字节密钥；配置只读 `APB_SERVER` / `APB_BIND` / `APB_KEY` / `APB_NAME` 环境变量或命令行参数。
- 默认端口 `30020`；实际以 `APB_SERVER` / `ss -tlnp` 为准。
- `exec` 命令在 agent 的 shell 中执行；服务端只路由，从不替节点执行命令。

## 本机现成环境速查（本机没有则忽略）

- 二进制：`command -v apb`；本机为 `/usr/local/bin/apb`，仓库构建产物为 `target/release/apb`。
- 服务端：systemd `apb-serve.service`；`systemctl status apb-serve --no-pager` 查看，端口以 `ss -tlnp | grep apb` 为准（本机当前 `0.0.0.0:30021`）。
- 密钥：`/root/.secrets/apb_key`（0600，只含 `APB_KEY=`）；用 `set -a; . /root/.secrets/apb_key; set +a` 加载，不要 `cat` / 回显。
- 本机控制：`APB_SERVER=127.0.0.1:30021`。
- 公网地址：GitHub 仓库 secret `APB_SERVER` 已保存；本机公网 IP 可用 `curl -fsS https://api.ipify.org` 获取，端口与监听端口一致。
- Actions 临时节点：`.github/workflows/apb-agent.yml`，secrets 已配置时用 `gh workflow run` 触发。

## AI 执行顺序

1. **定位二进制**：优先 `command -v apb`；本仓库用 `target/release/apb`；节点端没有二进制时用 `runApb.sh` 自动下载，不要先装 Rust / sshd。
2. **定位参数**：控制命令需要 `APB_SERVER` 和 `APB_KEY`；优先读环境变量，本机部署按上面的方法加载密钥文件。找不到就问用户，不要猜 IP / key，也不要把 key 打到输出。
3. **先 `status`**：`apb status --json`，从 `agents[].name` 拿节点名；单个 agent 在线可省略 `--name`，多个 agent 必须指定。
4. **再执行**：用 `apb exec/push/pull --json ...`；按 JSON 的 `ok` / `rc` 判断结果，不要解析人类可读输出。
5. **用完清理**：临时 agent / GitHub Actions run 不用了要及时停掉，避免残留占资源。

## 一、准备二进制

```bash
# 已有 apb；没有则用仓库里的构建产物
command -v apb || BIN=./target/release/apb

# 本仓库原生构建（cargo 不在 PATH 时先 export PATH="$HOME/.cargo/bin:$PATH"）
cargo build --release --locked
./target/release/apb --version

# 目标节点一键管理（自动识别 x86_64 / aarch64；无参数进入 1/2/3 数字菜单）
bash <(curl -fsSL https://raw.githubusercontent.com/BanYeHanFeng/apb/main/runApb.sh)
# 不支持进程替换的 shell：curl -fsSL https://raw.githubusercontent.com/BanYeHanFeng/apb/main/runApb.sh | bash
# 菜单：1 启动 / 2 停止 / 3 重启 / 4 更新 / 5 状态 / 6 修改配置 / 0 退出
# 也可直接执行：bash runApb.sh start|stop|restart|update|status|config

# 非交互 / 只安装 / 前台 / 预发布通道
# APB_SERVER='IP:30020' APB_KEY="$APB_KEY" APB_NAME=node-a APB_CHANNEL=prerelease \
#   bash runApb.sh --yes --background
# bash runApb.sh --install-only
# bash runApb.sh --foreground
# 注意：菜单/管理命令会把非密钥配置保存到 ~/.config/apb/agent.conf，APB_KEY 不落盘。
```

静态 musl 构建、CI 与发布说明见 `docs/构建与测试.md`。

## 二、服务端

```bash
# 临时起服务端：key 只显示一次，存到安全位置；已有部署不要重新生成
export APB_KEY="$(apb keygen)"
apb serve --bind 0.0.0.0:30020

# 本机已有 systemd 服务端
systemctl status apb-serve --no-pager
journalctl -u apb-serve -n 50 --no-pager
```

服务端只认证、注册、按名路由，不执行用户命令。

## 三、接入节点

```bash
# A. Termux / Android：无 root、无 sshd，一条命令打开管理菜单
bash <(curl -fsSL https://raw.githubusercontent.com/BanYeHanFeng/apb/main/runApb.sh)
# 首次选 1 启动：询问通道（1 正式版 / 2 预发布）、APB_SERVER、APB_KEY、节点名、运行方式
# 之后用 1 启动 / 2 停止 / 3 重启 / 4 更新 / 5 状态 / 6 配置 / 0 退出

# B. 已有 apb 的 Linux / runner：指定 APB_NAME 后启动
APB_SERVER='IP:30020' APB_KEY="$APB_KEY" APB_NAME='node-a' apb agent

# C. GitHub Actions 临时节点（runner 自带无密码 sudo，可提权；跑完即焚；在仓库目录执行）
gh workflow run apb-agent.yml -f ttl_minutes=120 -f name=gh-demo
# 不在仓库目录时加 -R BanYeHanFeng/apb
# 默认节点名为 <owner>-gh-<run_id>；不指定 -f name 时用它查 status
# 用完取消运行可立即销毁 runner：gh run cancel <run-id>
```

默认节点名是 `USER@HOSTNAME`，手机 / runner 间常重名；只要不是只有一个节点，就显式设置 `APB_NAME` / `--name`。

## 四、控制器

```bash
# 加载本机参数：控制器同样读 APB_SERVER / APB_KEY 环境变量
# 密钥文件不存在时跳过下一行，改用外部已有的 APB_KEY
set -a; . /root/.secrets/apb_key; set +a
: "${APB_SERVER:=127.0.0.1:30021}"   # 本机现成服务端；控制远程节点时改成公网 IP:端口
BIN=${APB_BIN:-apb}

# 1) 列节点；除 ok:true 外，还要确认 agents[].name
"$BIN" status --json
# {"ok":true,"server":"127.0.0.1:30021","count":1,"agents":[{"name":"phone-a",...}]}

# 2) 执行；-- 之后的参数按顺序拼接成一条命令，交给 agent 的 shell
"$BIN" exec --name phone-a --json -- 'uname -a; id; pwd'
"$BIN" exec --name phone-a --json --timeout 60 --cwd '~/site' -- 'ls -la'
"$BIN" exec --name phone-a --json --b64 -- 'head -c 32 /dev/urandom'
"$BIN" exec --name phone-a --json --raw -- '...'     # --raw：不经过 login shell

# 3) 传文件；远端路径的 ~ 由 agent 解析，整体加引号避免本机展开
"$BIN" push --name phone-a ./file '~/file'
"$BIN" push --name phone-a ./site '~/site' --json
"$BIN" pull --name phone-a '~/logs' ./logs --json
"$BIN" pull --name phone-a '~/out.log' ./ --json

# 4) 只测到服务端的 Noise 握手；不代表 agent 在线
"$BIN" doctor --json
```

`exec` 常用选项：

- `--timeout N`：agent 侧超时秒数，默认 300；`0` 表示 agent 不主动超时（控制端仍有 24h 上限）。
- `--cwd DIR`：agent 侧解析 `~`；目录不存在返回 `bad_cwd`、退出码 125，不会静默回退 `$HOME`。
- `--max-output N`：JSON 中每个流保留的最大字节数，默认 1 MiB；远端命令仍会被完整 drain 到结束。
- `--json` 优先；`--b64` 配合 `--json` 处理二进制输出。
- 不传 `--name` 时控制器会使用 `APB_NAME` 环境变量作为目标，注意不要和 agent 的节点名混淆。

## 五、结果语义与退出码

- `exec` 普通模式 stdout/stderr 实时透传，退出码 = 远端命令 rc；AI 应优先用 `--json`。
- `exec --json` 关键字段：`{"ok":true,"rc":0,"stdout":"...","stderr":"...","timed_out":false,"ended_reason":"ok"}`。
  `ok:true` 只表示执行完成，命令是否成功看 `rc`；真超时看 `timed_out:true`、`ended_reason:"timeout"`。
- `push` / `pull --json` 以 `ok`、`remote_path` / `local_path`、`bytes` 为准。
- `doctor` 只检查到服务端的 Noise 握手，不代表 agent 在线。

| 退出码 | 含义 |
|---|---|
| 0 | 成功 |
| N | exec 普通模式的远端 rc（信号为 128+signal） |
| 1 | 本地错误 / 文件传输本地失败 / 远端执行失败 |
| 2 | 缺少或非法 APB_KEY、缺少服务端地址 |
| 3 | 连接失败 / 协议错误 / 服务端错误 |
| 5 | 超时 |
| 6 | 文件传输失败 |
| 64 | 用法错误 |
| 125 | exec `--cwd` 目录不存在 |

## 六、Agent 连不上时

agent 会打印 `stage=resolve/connect/handshake/hello/hello_reply ... after Nms`：

- `stage=handshake ... failed to fill whole buffer` 且服务端日志 `noise read: decrypt error`：密钥不匹配。
- `os error 11`（EAGAIN）：本机 socket 失败 / 被拦截 / 超时，不是密钥错误。
- 重试间隔约 1 秒 = 每次立刻失败（本机或链路拒绝）；十几秒 = 在等超时。
- 手机 / Termux 优先排查 VPN / 代理（sing-box、Clash、v2rayNG）、省电与后台数据限制、私有 DNS，并换网络对照。
- 服务端与 agent 要用兼容版本；服务端日志出现 `peer did not send ... magic` 通常说明二进制 / 协议过旧。

详细排障见 `docs/故障排查.md`。

## 七、安全底线

- `APB_KEY` 泄露 = 节点可被接管：不要打印、不要写进仓库 / issue / 日志 / workflow inputs。
- 控制器优先用 `APB_KEY` 环境变量；`--key` 会出现在本机进程参数和 shell 历史中。
- 文件传输只写用户显式指定的路径；服务端只路由，命令只在 agent 的 shell 中执行。
