# 简介
将任意 Linux 节点接入远程执行 agent；连接只需要服务端地址 + 32 字节密钥

## 常见问题
**问：如何使用**
<p>
  <b>- 答：</b>把本项目链接粘贴给 AI <br>
</p>

## 一键客户端（数字菜单）
```bash
bash <(curl -fsSL https://raw.githubusercontent.com/BanYeHanFeng/apb/main/runApb.sh)
```

无参数运行会显示 1/2/3 数字菜单：

```text
1. 启动 apb agent
2. 停止 apb agent
3. 重启 apb agent
4. 更新 apb 二进制
5. 查看运行状态
6. 修改连接配置
0. 退出脚本
```

首次启动时脚本会自动检测 CPU 架构，并依次询问：
- 下载通道（1 正式版 latest / 2 预发布 pre-release，回车默认 1）
- 服务端地址（`IP:端口`，缺省端口 `30020`）
- `APB_KEY`（64 位 hex，输入不回显）
- 节点名（回车使用 `USER@HOSTNAME`）
- 运行方式（1 后台 / 2 前台，回车默认 1）

服务端地址、节点名、下载通道和运行方式会保存到 `~/.config/apb/agent.conf`（权限 600），方便下次启动和重启；`APB_KEY` 不写入配置文件，启动/重启时单独输入。

也可以跳过菜单直接执行命令：`start` / `stop` / `restart` / `update` / `status` / `config`（等价数字 `1`~`6`）。
非交互场景仍可按原方式启动，或使用 `--pre` / `--channel prerelease` / `APB_CHANNEL=prerelease` 直接选择预发布：

```bash
APB_SERVER=1.2.3.4:30020 APB_KEY=... APB_NAME=node-a \
  bash runApb.sh --yes --background
```

## 文档
| 文档 | 内容 |
|---|---|
| [`docs/使用手册.md`](docs/使用手册.md) | 架构、服务端 / Agent / 控制器、命令与文件传输契约、Actions 临时节点 |
| [`docs/构建与测试.md`](docs/构建与测试.md) | 原生构建、静态 musl 交叉编译、构建工作流、测试与发布 |
| [`docs/协议.md`](docs/协议.md) | APB0 线协议说明（apb 0.1.0） |
| [`docs/安全.md`](docs/安全.md) | 威胁模型与加固说明 |
| [`docs/故障排查.md`](docs/故障排查.md) | 常见故障与排障 |
| [`skills/apb/SKILL.md`](skills/apb/SKILL.md) | AI 可执行的速用手册 |

## 安全
`APB_KEY` 泄露等价于节点被接管，请通过环境变量 / GitHub Actions secrets 传递
