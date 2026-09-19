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

直接运行会显示数字菜单（脚本不接受任何命令行选项）：

```text
1. 启动 apb
2. 停止 apb
3. 重启 apb
4. 安装 apb（选择正式版 / 预发布版）
5. 更新 apb
6. 查看运行状态
7. 修改连接配置
0. 退出脚本
```

选择菜单中的 `4. 安装 apb` 时，脚本会：
- 自动检测 CPU 架构（`x86_64` / `aarch64`）
- 询问安装通道（`1` 正式版 latest / `2` 预发布版 pre-release，需要明确选择）
- 下载并覆盖安装到安装目录，不启动 agent

> 启动 / 重启只使用已经安装好的 `apb`，不会自动下载。未安装时会提示先返回菜单选择 `4. 安装 apb`。

安装后选择 `1. 启动 apb` 时，脚本才会依次询问：
- 服务端地址（`IP:端口`，缺省端口 `30020`）
- `APB_KEY`（64 位 hex，输入不回显）
- 节点名（回车使用 `USER@HOSTNAME`）
- 运行方式（1 后台 / 2 前台，回车默认 1）

服务端地址、节点名、下载通道和运行方式会保存到 `~/.config/apb/agent.conf`（权限 600），方便下次启动和重启；`APB_KEY` 不写入配置文件，启动/重启时单独输入。

`runApb.sh` 只支持“无参数 + 交互式终端”的运行方式，不接受任何命令行选项；安装、启动、更新等操作都请按数字菜单提示完成。

如所在网络无法直接访问 GitHub，可在运行脚本时通过环境变量指定镜像前缀，再在菜单中选择 `4. 安装 apb`：

```bash
APB_GH_PROXY='https://gh-proxy.example' \
  bash <(curl -fsSL https://raw.githubusercontent.com/BanYeHanFeng/apb/main/runApb.sh)
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
