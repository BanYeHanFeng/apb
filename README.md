# 简介
将任意 Linux 节点接入远程执行 agent；连接只需要服务端地址 + 32 字节密钥

## 常见问题
**问：如何使用**
<p>
  <b>- 答：</b>把本项目链接粘贴给 AI <br>
</p>

## 一键客户端
```bash
bash <(curl -fsSL https://raw.githubusercontent.com/BanYeHanFeng/apb/main/runApb.sh)
```

脚本会自动检测 CPU 架构、下载对应的静态 `apb`，然后依次询问：
- 服务端地址（`IP:端口`，缺省端口 `30020`）
- `APB_KEY`（64 位 hex，输入不回显）
- 节点名（回车使用 `USER@HOSTNAME`）
- 是否后台运行（回车默认 `Y`）

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
