#!/usr/bin/env bash
#
# apb 客户端综合管理脚本（1/2/3 数字菜单）：
#   启动 / 停止 / 重启 agent、更新 apb 二进制、查看状态、修改连接配置。
#
# 推荐用法（stdin 仍留给终端，脚本读取 /dev/tty，所以可以放心交互）：
#   bash <(curl -fsSL https://raw.githubusercontent.com/BanYeHanFeng/apb/main/runApb.sh)
#   无参数运行会显示数字菜单，适合日常管理。
#
# 非交互 / CI 用法（保持原有一次性启动方式）：
#   APB_SERVER=1.2.3.4:30020 APB_KEY=64位hex APB_NAME=node-a \
#     bash runApb.sh --yes --background
#
# 只安装/更新二进制，不启动：
#   bash runApb.sh --install-only
#   bash runApb.sh update
#
# 环境变量（除了与 apb 一致的前三个，其余都只影响本脚本）：
#   APB_SERVER / APB_KEY / APB_NAME   服务端地址、密钥、节点名
#   APB_CONFIG                        管理配置文件，默认 ~/.config/apb/agent.conf
#   APB_CONFIG_DIR                    管理配置目录，默认 ~/.config/apb
#   APB_PID_FILE                      agent PID 文件，默认 <配置目录>/agent.pid
#   APB_INSTALL_DIR                   二进制安装目录，默认 ~/.local/bin
#   APB_BIN                           已存在的 apb 二进制，设置后跳过下载
#   APB_BINARY_URL                    直接指定二进制下载地址
#   APB_REPO                          GitHub 仓库，默认 BanYeHanFeng/apb
#   APB_CHANNEL                       下载通道，stable（默认）/ prerelease；未设置时交互脚本会询问
#   APB_PRE_RELEASE_TAG               预发布 Release 标签，默认 pre-release
#   APB_VERSION                       Release 版本，默认 latest；填写具体 v* 时优先于通道
#   APB_RELEASE_BASE                  Release 下载页，默认 https://github.com/$APB_REPO/releases
#   APB_GH_PROXY                      GitHub 加速前缀，按 <前缀>/<完整URL> 拼装
#   APB_LOG                           后台日志路径，默认可写的临时目录或 HOME 下 apb-agent-<name>.log
#   APB_BACKGROUND=1                  等价于 --background
#   APB_YES=1                         等价于 --yes
#   APB_UPDATE=1                      等价于 --update，强制重新下载
#   APB_TARGET                        覆盖自动检测的 Rust target / 资产后缀
#
# 说明：
#   脚本会把服务端地址、节点名、下载通道、运行方式等非密钥信息保存到 APB_CONFIG
#   （权限 600），方便下次启动 / 重启；APB_KEY 不写入配置文件，只在启动/重启时
#   通过 /dev/tty 无回显读取后以环境变量传给 agent。
#

set -Eeuo pipefail

DEFAULT_REPO="${APB_REPO:-BanYeHanFeng/apb}"
DEFAULT_PORT=30020
DEFAULT_INSTALL_DIR="${APB_INSTALL_DIR:-${HOME:-.}/.local/bin}"
DEFAULT_RELEASE_BASE="${APB_RELEASE_BASE:-https://github.com/${DEFAULT_REPO}/releases}"
ASSUME_YES="${APB_YES:-0}"
BACKGROUND_MODE="${APB_BACKGROUND:-ask}"   # ask / 1 / 0
INSTALL_ONLY=0
FORCE_UPDATE="${APB_UPDATE:-0}"
BIN_OVERRIDE="${APB_BIN:-}"
BINARY_URL="${APB_BINARY_URL:-}"
SERVER="${APB_SERVER:-}"
KEY="${APB_KEY:-}"
NAME="${APB_NAME:-}"
TARGET_OVERRIDE="${APB_TARGET:-}"
CHANNEL="${APB_CHANNEL:-stable}"
CHANNEL_EXPLICIT=0
if [ -n "${APB_CHANNEL:-}" ]; then
  CHANNEL_EXPLICIT=1
fi
# 与 .github/workflows/ci.yml 的 PRERELEASE_TAG 保持一致；CI 用它维护唯一的预发布 Release。
PRE_RELEASE_TAG="${APB_PRE_RELEASE_TAG:-pre-release}"
VERSION="${APB_VERSION:-latest}"
RELEASE_BASE="${DEFAULT_RELEASE_BASE%/}"
GH_PROXY="${APB_GH_PROXY:-}"
REQUESTED_SERVER=""
REQUESTED_KEY=""
REQUESTED_NAME=""
APB_BIN_PATH=""
APB_BIN_VERSION=""
SERVER_NORM=""
TMP_FILE=""
LOG_FILE=""
SCRIPT_DIR=""
SOURCE_FILE="${BASH_SOURCE[0]:-$0}"
if [ -n "$SOURCE_FILE" ] && [ -f "$SOURCE_FILE" ]; then
  SCRIPT_DIR="$(cd -- "$(dirname -- "$SOURCE_FILE")" 2>/dev/null && pwd)" || SCRIPT_DIR=""
fi

# 管理配置 / PID 文件路径。CONFIG_FILE 默认 ~/.config/apb/agent.conf。
CONFIG_FILE="${APB_CONFIG:-${APB_CONFIG_DIR:-${XDG_CONFIG_HOME:-${HOME:-.}/.config}/apb}/agent.conf}"
CONFIG_BASE_DIR="$(dirname -- "$CONFIG_FILE" 2>/dev/null || printf '.')"
[ -n "$CONFIG_BASE_DIR" ] || CONFIG_BASE_DIR="."
PID_FILE="${APB_PID_FILE:-${CONFIG_BASE_DIR}/agent.pid}"

# 运行模式：menu / legacy / start / stop / restart / update / status / config。
ACTION=""
PERSIST_CONFIG=0
CONFIG_LOADED=0
INSTALL_DIR_EXPLICIT=0

# 只允许从终端读取输入；没有终端时所有参数都必须通过环境变量/参数提供。
INPUT_FROM_TTY=0
if { : </dev/tty; } 2>/dev/null; then
  INPUT_FROM_TTY=1
elif [ -t 0 ]; then
  INPUT_FROM_TTY=2
fi

info()  { printf '[*] %s\n' "$*"; }
ok()    { printf '[+] %s\n' "$*"; }
warn()  { printf '[!] %s\n' "$*" >&2; }
error() { printf '[X] %s\n' "$*" >&2; }
die()   { error "$*"; exit 1; }

cleanup() {
  if [ -n "${TMP_FILE:-}" ] && [ -e "$TMP_FILE" ]; then
    rm -f -- "$TMP_FILE"
  fi
}
trap cleanup EXIT

usage() {
  cat <<EOF
apb 客户端综合管理脚本（数字菜单版）

用法:
  bash <(curl -fsSL https://raw.githubusercontent.com/${DEFAULT_REPO}/main/runApb.sh)
                                   # 无参数且终端可用时显示 1/2/3 数字菜单
  bash runApb.sh [命令] [选项]

命令:
  1 | start      启动 apb agent
  2 | stop       停止 apb agent
  3 | restart    重启 apb agent
  4 | update     重新下载最新 apb 二进制
  5 | status     查看运行状态
  6 | config     修改连接配置（APB_KEY 仅本次会话生效，不落盘）
  0 | menu       显示数字菜单

本脚本会:
  1. 检测 Linux 架构（x86_64 / aarch64）；
  2. 交互选择下载通道（1 正式版 / 2 预发布，回车默认正式版；非交互时用选项或环境变量指定）；
  3. 下载对应的静态 apb 到安装目录；
  4. 交互询问服务端地址、APB_KEY、节点名和运行方式；
  5. 通过数字菜单启动、停止、重启、更新和查看 agent。

选项:
  -s, --server IP:PORT   服务端地址，缺省端口 ${DEFAULT_PORT}
  -k, --key KEY          64 位 hex 密钥（或 base64:...；会留在 shell 历史，建议用 APB_KEY）
  -n, --name NAME        节点名
  -b, --background       后台运行（默认）
  -f, --foreground       前台运行，Ctrl+C 停止
  -y, --yes              不交互，使用已有参数 / 默认运行方式；缺少必填项则报错
  -c, --channel CHANNEL  下载通道：stable 正式版（默认）/ prerelease 预发布
      --stable           等价于 --channel stable
      --pre, --prerelease
                         等价于 --channel prerelease
      --install-only     只下载/校验二进制，不启动 agent
      --update           强制重新下载最新二进制
      --menu             强制显示数字菜单
      --binary PATH      使用已有 apb 二进制，不下载
      --repo OWNER/REPO  指定 GitHub 仓库，默认 ${DEFAULT_REPO}
      --release-base URL 指定 Release 地址前缀
      --install-dir DIR  指定安装目录，默认 ${DEFAULT_INSTALL_DIR}
      --target TARGET    覆盖自动检测的 Release 资产后缀
      --binary-url URL   直接指定二进制下载地址，跳过 Release 拼接
  -h, --help             显示帮助
      --version          显示本脚本版本

示例:
  bash runApb.sh                     # 打开管理菜单
  bash runApb.sh start               # 下载（如需要）并启动 agent
  bash runApb.sh 5                   # 查看运行状态
  APB_SERVER=1.2.3.4:30020 APB_KEY=\$(apb keygen) APB_NAME=phone-a \\
    APB_CHANNEL=prerelease bash runApb.sh --yes --background

配置与安全:
  无参数进入菜单（或执行 start/restart/config 命令）后，会把服务端地址、节点名、下载通道、
  运行方式等非密钥信息保存到 ${CONFIG_FILE}（权限 600）。
  APB_KEY 不回显、不写入配置文件，只在启动/重启时读取并通过环境变量传给 agent。
  不要把密钥写进 issue、日志或公开仓库；泄露后请立即轮换服务端与所有 agent 的密钥。
EOF
}

version() {
  printf 'runApb.sh (apb client manager)\n'
}

normalize_channel() {
  local value
  value="$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')"
  case "$value" in
    stable|release|latest|正式版|正式|稳定版|稳定)
      printf 'stable'
      ;;
    prerelease|pre-release|pre|preview|beta|canary|nightly|edge|预发布版|预发布|预览版|预览)
      printf 'prerelease'
      ;;
    *)
      return 1
      ;;
  esac
}

channel_name() {
  if [ "$CHANNEL" = "prerelease" ]; then
    printf '预发布'
  else
    printf '正式版'
  fi
}

parse_args() {
  while [ "$#" -gt 0 ]; do
    case "$1" in
      -s|--server)
        [ "$#" -ge 2 ] || die "$1 缺少参数"
        REQUESTED_SERVER="$2"; shift 2
        ;;
      --server=*)
        REQUESTED_SERVER="${1#*=}"; shift
        ;;
      -k|--key)
        [ "$#" -ge 2 ] || die "$1 缺少参数"
        REQUESTED_KEY="$2"; shift 2
        ;;
      --key=*)
        REQUESTED_KEY="${1#*=}"; shift
        ;;
      -n|--name)
        [ "$#" -ge 2 ] || die "$1 缺少参数"
        REQUESTED_NAME="$2"; shift 2
        ;;
      --name=*)
        REQUESTED_NAME="${1#*=}"; shift
        ;;
      -b|--background)
        BACKGROUND_MODE=1; shift
        ;;
      -f|--foreground)
        BACKGROUND_MODE=0; shift
        ;;
      -y|--yes)
        ASSUME_YES=1; shift
        ;;
      --install-only)
        INSTALL_ONLY=1; shift
        ;;
      --update)
        FORCE_UPDATE=1; shift
        ;;
      --menu)
        ACTION="menu"; shift
        ;;
      --binary)
        [ "$#" -ge 2 ] || die "$1 缺少参数"
        BIN_OVERRIDE="$2"; shift 2
        ;;
      --binary=*)
        BIN_OVERRIDE="${1#*=}"; shift
        ;;
      --install-dir)
        [ "$#" -ge 2 ] || die "$1 缺少参数"
        DEFAULT_INSTALL_DIR="$2"; INSTALL_DIR_EXPLICIT=1; shift 2
        ;;
      --install-dir=*)
        DEFAULT_INSTALL_DIR="${1#*=}"; INSTALL_DIR_EXPLICIT=1; shift
        ;;
      --target)
        [ "$#" -ge 2 ] || die "$1 缺少参数"
        TARGET_OVERRIDE="$2"; shift 2
        ;;
      --target=*)
        TARGET_OVERRIDE="${1#*=}"; shift
        ;;
      --binary-url)
        [ "$#" -ge 2 ] || die "$1 缺少参数"
        BINARY_URL="$2"; shift 2
        ;;
      --binary-url=*)
        BINARY_URL="${1#*=}"; shift
        ;;
      --repo)
        [ "$#" -ge 2 ] || die "$1 缺少参数"
        DEFAULT_REPO="$2"
        RELEASE_BASE="${APB_RELEASE_BASE:-https://github.com/${DEFAULT_REPO}/releases}"
        RELEASE_BASE="${RELEASE_BASE%/}"
        shift 2
        ;;
      --repo=*)
        DEFAULT_REPO="${1#*=}"
        RELEASE_BASE="${APB_RELEASE_BASE:-https://github.com/${DEFAULT_REPO}/releases}"
        RELEASE_BASE="${RELEASE_BASE%/}"
        shift
        ;;
      --release-base)
        [ "$#" -ge 2 ] || die "$1 缺少参数"
        RELEASE_BASE="${2%/}"; shift 2
        ;;
      --release-base=*)
        RELEASE_BASE="${1#*=}"; RELEASE_BASE="${RELEASE_BASE%/}"; shift
        ;;
      -c|--channel)
        [ "$#" -ge 2 ] || die "$1 缺少参数"
        CHANNEL="$2"; CHANNEL_EXPLICIT=1; shift 2
        ;;
      --channel=*)
        CHANNEL="${1#*=}"; CHANNEL_EXPLICIT=1; shift
        ;;
      --stable)
        CHANNEL="stable"; CHANNEL_EXPLICIT=1; shift
        ;;
      --pre|--prerelease)
        CHANNEL="prerelease"; CHANNEL_EXPLICIT=1; shift
        ;;
      -h|--help)
        usage; exit 0
        ;;
      --version)
        version; exit 0
        ;;
      --)
        shift
        [ "$#" -eq 0 ] || die "未知参数: $1"
        break
        ;;
      -*)
        die "未知参数: $1"
        ;;
      *)
        die "未知位置参数: $1"
        ;;
    esac
  done

  [ -n "$REQUESTED_SERVER" ] && SERVER="$REQUESTED_SERVER"
  [ -n "$REQUESTED_KEY" ] && KEY="$REQUESTED_KEY"
  [ -n "$REQUESTED_NAME" ] && NAME="$REQUESTED_NAME"

  case "$ASSUME_YES" in 1|true|TRUE|yes|YES) ASSUME_YES=1 ;; *) ASSUME_YES=0 ;; esac
  case "$BACKGROUND_MODE" in
    1|true|TRUE|yes|YES) BACKGROUND_MODE=1 ;;
    0|false|FALSE|no|NO) BACKGROUND_MODE=0 ;;
    ask|"") BACKGROUND_MODE=ask ;;
    *) die "APB_BACKGROUND 值无效: $BACKGROUND_MODE" ;;
  esac
  case "$FORCE_UPDATE" in 1|true|TRUE|yes|YES) FORCE_UPDATE=1 ;; *) FORCE_UPDATE=0 ;; esac

  local normalized_channel
  if ! normalized_channel="$(normalize_channel "$CHANNEL")"; then
    die "下载通道无效: ${CHANNEL}（可选 stable 正式版 / prerelease 预发布）"
  fi
  CHANNEL="$normalized_channel"
}

trim() {
  local s="$1"
  s="${s%$'\r'}"
  s="${s#"${s%%[![:space:]]*}"}"
  s="${s%"${s##*[![:space:]]}"}"
  printf '%s' "$s"
}

detect_target() {
  if [ -n "$TARGET_OVERRIDE" ]; then
    printf '%s' "$TARGET_OVERRIDE"
    return 0
  fi
  local machine
  machine="$(uname -m 2>/dev/null || true)"
  case "$machine" in
    x86_64|amd64|x64)
      printf '%s' "x86_64-unknown-linux-musl"
      ;;
    aarch64|arm64)
      printf '%s' "aarch64-unknown-linux-musl"
      ;;
    *)
      printf '%s' ""
      ;;
  esac
}

validate_binary() {
  local path="$1" out
  [ -f "$path" ] || return 1
  [ -x "$path" ] || return 1
  out="$("$path" --version 2>/dev/null)" || return 1
  case "$out" in
    apb\ *) APB_BIN_VERSION="$out"; return 0 ;;
    *) return 1 ;;
  esac
}

resolve_binary() {
  local target="$1" candidate existing="" mark_file="" existing_channel=""

  if [ -n "$BIN_OVERRIDE" ]; then
    BIN_OVERRIDE="$(cd -- "$(dirname -- "$BIN_OVERRIDE")" 2>/dev/null && pwd)/$(basename -- "$BIN_OVERRIDE")"
    if ! validate_binary "$BIN_OVERRIDE"; then
      die "指定的 APB_BIN 不可用或不是 apb 二进制: $BIN_OVERRIDE"
    fi
    APB_BIN_PATH="$BIN_OVERRIDE"
    return 0
  fi

  # 显式指定 BINARY_URL 时，直接下载该地址，不优先使用本地缓存。
  if [ -n "$BINARY_URL" ]; then
    download_binary "$target"
    return 0
  fi

  # 默认正式版仍优先使用脚本目录 / 当前目录里已经构建好的 apb，方便仓库内直接运行；
  # 显式指定下载通道时直接下载对应 Release，避免误用本地与通道不符的构建。
  if [ "$FORCE_UPDATE" -ne 1 ] && [ "$CHANNEL_EXPLICIT" -ne 1 ]; then
    local -a candidates=()
    [ -n "$SCRIPT_DIR" ] && candidates+=("$SCRIPT_DIR/apb")
    candidates+=("./apb")
    candidates+=("$PWD/apb")
    for candidate in "${candidates[@]}"; do
      if validate_binary "$candidate"; then
        APB_BIN_PATH="$(cd -- "$(dirname -- "$candidate")" && pwd)/$(basename -- "$candidate")"
        return 0
      fi
    done
  fi

  if [ -n "${DEFAULT_INSTALL_DIR:-}" ]; then
    mkdir -p -- "$DEFAULT_INSTALL_DIR" 2>/dev/null || true
    existing="$DEFAULT_INSTALL_DIR/apb"
    mark_file="${DEFAULT_INSTALL_DIR%/}/.apb-channel"
    if [ "$FORCE_UPDATE" -eq 0 ] && validate_binary "$existing"; then
      if [ -f "$mark_file" ]; then
        existing_channel="$(cat -- "$mark_file" 2>/dev/null || true)"
      fi
      if [ "$existing_channel" = "$CHANNEL" ]; then
        APB_BIN_PATH="$existing"
        return 0
      fi
      # 兼容旧脚本安装的、没有通道标记的二进制：默认正式版继续复用。
      if [ -z "$existing_channel" ] && [ "$CHANNEL" = "stable" ] && [ "$CHANNEL_EXPLICIT" -eq 0 ]; then
        APB_BIN_PATH="$existing"
        return 0
      fi
    fi
  fi

  if command -v curl >/dev/null 2>&1; then
    :
  elif command -v wget >/dev/null 2>&1; then
    :
  else
    die "未找到 curl 或 wget，无法下载 apb"
  fi

  download_binary "$target"
}

download_url_for() {
  local target="$1" asset url
  asset="apb-${target}"
  if [ -n "$BINARY_URL" ]; then
    url="$BINARY_URL"
  elif [ -n "$VERSION" ] && [ "$VERSION" != "latest" ]; then
    # 显式指定具体版本时优先于下载通道
    url="${RELEASE_BASE}/download/${VERSION}/${asset}"
  elif [ "$CHANNEL" = "prerelease" ]; then
    url="${RELEASE_BASE}/download/${PRE_RELEASE_TAG}/${asset}"
  else
    url="${RELEASE_BASE}/latest/download/${asset}"
  fi
  if [ -n "$GH_PROXY" ] && [ -z "$BINARY_URL" ]; then
    url="${GH_PROXY%/}/${url}"
  fi
  printf '%s' "$url"
}

download_file() {
  local url="$1" dest="$2"
  local -a curl_args wget_args
  if command -v curl >/dev/null 2>&1; then
    curl_args=(-fL --retry 3 --connect-timeout 15 --max-time 300 -o "$dest")
    if [ -t 2 ]; then
      curl_args+=(--progress-bar)
    else
      curl_args+=(-sS)
    fi
    curl "${curl_args[@]}" "$url"
  elif command -v wget >/dev/null 2>&1; then
    wget_args=(-O "$dest")
    if [ ! -t 2 ]; then
      wget_args=(-q "${wget_args[@]}")
    fi
    wget "${wget_args[@]}" "$url"
  else
    return 1
  fi
}

download_binary() {
  local target="$1" url install_dir mark_file mark_tmp asset
  install_dir="$DEFAULT_INSTALL_DIR"
  [ -n "$install_dir" ] || die "无法确定安装目录"

  if ! mkdir -p -- "$install_dir" 2>/dev/null; then
    die "无法创建安装目录: $install_dir"
  fi
  if [ ! -w "$install_dir" ]; then
    die "安装目录不可写: $install_dir"
  fi

  if [ -z "$target" ] && [ -z "$BINARY_URL" ]; then
    die "不支持的架构: $(uname -m 2>/dev/null || printf 'unknown')"
  fi

  url="$(download_url_for "$target")"

  TMP_FILE="$install_dir/.apb-download.$$.$RANDOM"
  rm -f -- "$TMP_FILE"
  if [ -n "$BINARY_URL" ]; then
    asset="$(basename -- "${url%%\?*}")"
    [ -n "$asset" ] || asset="自定义二进制"
    info "准备从自定义地址下载 apb: ${asset}"
    info "下载地址: ${url}"
  else
    asset="apb-${target}"
    info "检测到架构 ${target}，下载通道: $(channel_name)"
    if [ "$CHANNEL" = "prerelease" ] && [ "$VERSION" = "latest" ]; then
      info "预发布 Release: ${PRE_RELEASE_TAG}（仓库中只保留一个）"
    fi
    info "准备下载 ${asset}"
    info "下载地址: ${url}"
  fi

  if ! download_file "$url" "$TMP_FILE"; then
    rm -f -- "$TMP_FILE"
    TMP_FILE=""
    die "下载 apb 失败: ${url}"
  fi

  chmod 0755 -- "$TMP_FILE" 2>/dev/null || true
  if ! validate_binary "$TMP_FILE"; then
    rm -f -- "$TMP_FILE"
    TMP_FILE=""
    die "下载的 apb 无法运行: ${url}"
  fi

  if ! mv -f -- "$TMP_FILE" "$install_dir/apb" 2>/dev/null; then
    die "无法写入 ${install_dir}/apb"
  fi
  TMP_FILE=""
  APB_BIN_PATH="$install_dir/apb"

  # 记录安装目录里的二进制来自哪个通道，显式切换通道时不会被旧缓存误用。
  mark_file="${install_dir%/}/.apb-channel"
  mark_tmp="${install_dir%/}/.apb-channel.$$.$RANDOM"
  if printf '%s\n' "$CHANNEL" > "$mark_tmp" 2>/dev/null && mv -f -- "$mark_tmp" "$mark_file" 2>/dev/null; then
    :
  else
    rm -f -- "$mark_tmp" 2>/dev/null || true
    warn "无法写入下载通道标记: $mark_file（下次显式切换通道时可能重新下载）"
  fi
}

prompt_read() {
  local var="$1" prompt="$2" silent="${3:-0}" line=""
  if [ "$INPUT_FROM_TTY" -eq 1 ]; then
    if [ "$silent" -eq 1 ]; then
      printf '%s' "$prompt" >&2
      if ! IFS= read -r -s line < /dev/tty; then
        printf '\n' >&2
        return 1
      fi
      printf '\n' >&2
    else
      if ! IFS= read -r -p "$prompt" line < /dev/tty; then
        return 1
      fi
    fi
  elif [ "$INPUT_FROM_TTY" -eq 2 ]; then
    if [ "$silent" -eq 1 ]; then
      printf '%s' "$prompt" >&2
      if ! IFS= read -r -s line; then
        printf '\n' >&2
        return 1
      fi
      printf '\n' >&2
    else
      if ! IFS= read -r -p "$prompt" line; then
        return 1
      fi
    fi
  else
    return 1
  fi
  printf -v "$var" '%s' "$line"
}

normalize_server() {
  local raw host port ipv6=0 p
  raw="$(trim "$1")"
  [ -n "$raw" ] || return 1
  [[ "$raw" != *[[:space:]]* ]] || return 1
  case "$raw" in
    *://*) return 1 ;;
  esac

  if [[ "$raw" =~ ^\[([0-9A-Fa-f:.]+)\]:([0-9]{1,5})$ ]]; then
    host="${BASH_REMATCH[1]}"; port="${BASH_REMATCH[2]}"
  elif [[ "$raw" =~ ^\[([0-9A-Fa-f:.]+)\]$ ]]; then
    host="${BASH_REMATCH[1]}"; port="$DEFAULT_PORT"
  elif [[ "$raw" =~ ^([A-Za-z0-9._-]+):([0-9]{1,5})$ ]]; then
    host="${BASH_REMATCH[1]}"; port="${BASH_REMATCH[2]}"
  elif [[ "$raw" =~ ^[A-Za-z0-9._-]+$ ]]; then
    host="$raw"; port="$DEFAULT_PORT"
  elif [[ "$raw" == *:* ]] && [[ "$raw" =~ ^[0-9A-Fa-f:.]+$ ]]; then
    host="$raw"; port="$DEFAULT_PORT"; ipv6=1
  else
    return 1
  fi

  p=$((10#$port))
  if [ "$p" -lt 1 ] || [ "$p" -gt 65535 ]; then
    return 1
  fi
  if [ "$ipv6" -eq 1 ] || [[ "$host" == *:* ]]; then
    SERVER_NORM="[$host]:$p"
  else
    SERVER_NORM="${host}:${p}"
  fi
  return 0
}

validate_key() {
  local key="$1" body
  [ -n "$key" ] || return 1
  key="$(trim "$key")"
  case "$key" in
    hex:*|HEX:*)
      body="${key#*:}"
      [[ "$body" =~ ^[0-9A-Fa-f]{64}$ ]]
      ;;
    base64:*|BASE64:*)
      body="${key#*:}"
      [ "${#body}" -ge 43 ] && [ "${#body}" -le 44 ] && [[ "$body" =~ ^[A-Za-z0-9+/_-]+={0,2}$ ]]
      ;;
    *)
      [[ "$key" =~ ^[0-9A-Fa-f]{64}$ ]]
      ;;
  esac
}

validate_name() {
  local name="$1"
  [ -n "$name" ] || return 1
  [ "${#name}" -le 128 ] || return 1
  [[ "$name" =~ ^[A-Za-z0-9._@:+~-]+$ ]]
}

default_name() {
  local user host
  user="$(id -un 2>/dev/null || printf 'agent')"
  host="$(hostname 2>/dev/null || printf 'node')"
  host="${host%%.*}"
  case "$host" in
    ""|*[!A-Za-z0-9._-]*) host="node" ;;
  esac
  printf '%s@%s' "$user" "$host"
}

ask_channel() {
  local ans
  if [ "$CHANNEL_EXPLICIT" -eq 1 ] || [ -n "$BIN_OVERRIDE" ] || [ -n "$BINARY_URL" ]; then
    return 0
  fi
  # 已显式指定具体 Release 版本时，通道不再影响下载地址。
  if [ -n "$VERSION" ] && [ "$VERSION" != "latest" ]; then
    return 0
  fi
  if [ "$ASSUME_YES" -eq 1 ] || [ "$INPUT_FROM_TTY" -eq 0 ]; then
    CHANNEL="stable"
    return 0
  fi

  while :; do
    prompt_read ans "请输入下载通道（1=正式版 latest，2=预发布 pre-release，回车默认 1）: " 0 \
      || die "没有可用的终端输入，无法询问下载通道"
    ans="$(trim "$ans")"
    case "${ans:-1}" in
      1|stable|release|正式版|正式)
        CHANNEL="stable"
        return 0
        ;;
      2|pre|pre-release|prerelease|preview|beta|预发布|预发布版|预览|预览版)
        CHANNEL="prerelease"
        # 用户主动选择预发布时跳过本地 / 旧缓存二进制，确保从预发布 Release 下载。
        CHANNEL_EXPLICIT=1
        return 0
        ;;
      *)
        warn "输入无效: ${ans}（请输入 1 或 2）"
        ;;
    esac
  done
}

ask_server() {
  local ans
  while :; do
    if [ "$ASSUME_YES" -eq 1 ] && [ -z "$SERVER" ]; then
      die "缺少服务端地址"
    fi
    if [ -n "$SERVER" ]; then
      if normalize_server "$SERVER"; then
        SERVER="$SERVER_NORM"
        return 0
      fi
      if [ "$INPUT_FROM_TTY" -eq 0 ] || [ "$ASSUME_YES" -eq 1 ]; then
        die "APB_SERVER 格式非法: $SERVER"
      fi
      warn "已有 APB_SERVER 格式非法: $SERVER"
      SERVER=""
    fi
    if [ "$INPUT_FROM_TTY" -eq 0 ]; then
      die "缺少服务端地址"
    fi
    prompt_read ans "请输入服务端地址（IP:端口，可省略端口，默认 ${DEFAULT_PORT}）: " 0 || die "没有可用的终端输入，无法询问 APB_SERVER"
    ans="$(trim "$ans")"
    if [ -z "$ans" ]; then
      warn "服务端地址不能为空。"
      continue
    fi
    if ! normalize_server "$ans"; then
      warn "服务端地址格式不正确: $ans"
      continue
    fi
    SERVER="$SERVER_NORM"
    return 0
  done
}

ask_key() {
  local ans
  while :; do
    if [ "$ASSUME_YES" -eq 1 ] && [ -z "$KEY" ]; then
      die "缺少 APB_KEY"
    fi
    if [ -n "$KEY" ]; then
      if validate_key "$KEY"; then
        return 0
      fi
      if [ "$INPUT_FROM_TTY" -eq 0 ] || [ "$ASSUME_YES" -eq 1 ]; then
        die "APB_KEY 格式非法"
      fi
      warn "已有 APB_KEY 格式非法"
      KEY=""
    fi
    if [ "$INPUT_FROM_TTY" -eq 0 ]; then
      die "缺少 APB_KEY"
    fi
    prompt_read ans "请输入 APB_KEY（64 位 hex，输入不回显）: " 1 || die "没有可用的终端输入，无法询问 APB_KEY"
    ans="$(trim "$ans")"
    if [ -z "$ans" ]; then
      warn "APB_KEY 不能为空。"
      continue
    fi
    if ! validate_key "$ans"; then
      warn "APB_KEY 格式不正确"
      continue
    fi
    KEY="$ans"
    return 0
  done
}

ask_name() {
  local fallback ans
  fallback="$(default_name)"
  while :; do
    if [ "$ASSUME_YES" -eq 1 ] && [ -z "$NAME" ]; then
      NAME="$fallback"
      return 0
    fi
    if [ -n "$NAME" ]; then
      NAME="$(trim "$NAME")"
      if validate_name "$NAME"; then
        return 0
      fi
      if [ "$INPUT_FROM_TTY" -eq 0 ] || [ "$ASSUME_YES" -eq 1 ]; then
        die "APB_NAME 格式非法: $NAME"
      fi
      warn "已有 APB_NAME 格式非法: $NAME"
      NAME=""
    fi
    if [ "$INPUT_FROM_TTY" -eq 0 ]; then
      NAME="$fallback"
      return 0
    fi
    prompt_read ans "请输入节点名称（回车使用 ${fallback}）: " 0 || die "没有可用的终端输入，无法询问 APB_NAME"
    ans="$(trim "$ans")"
    if [ -z "$ans" ]; then
      NAME="$fallback"
      return 0
    fi
    if ! validate_name "$ans"; then
      warn "节点名格式不正确: $ans"
      continue
    fi
    NAME="$ans"
    return 0
  done
}

ask_background() {
  local ans
  case "$BACKGROUND_MODE" in
    1|0) return 0 ;;
  esac

  if [ "$ASSUME_YES" -eq 1 ] || [ "$INPUT_FROM_TTY" -eq 0 ]; then
    BACKGROUND_MODE=1
    return 0
  fi

  while :; do
    prompt_read ans "请输入运行方式（1 后台运行，2 前台运行，回车默认 1）: " 0 || die "没有可用的终端输入，无法询问运行方式"
    ans="$(trim "$ans")"
    case "${ans:-1}" in
      1|y|Y|yes|YES|Yes|后台) BACKGROUND_MODE=1; return 0 ;;
      2|n|N|no|NO|No|前台)   BACKGROUND_MODE=0; return 0 ;;
      *) warn "输入无效: ${ans}（请输入 1 或 2）" ;;
    esac
  done
}

sanitize_log_name() {
  printf '%s' "$1" | tr -c 'A-Za-z0-9._-' '_'
}

# ================= 配置读写 =================

config_dir() {
  case "$CONFIG_FILE" in
    */*) printf '%s\n' "${CONFIG_FILE%/*}" ;;
    *)   printf '.\n' ;;
  esac
}

ensure_config_dir() {
  local dir
  dir="$(config_dir)"
  # 只对本次新建的目录收紧权限，避免把 /tmp、/etc 等自定义路径的父目录改坏。
  if [ -d "$dir" ]; then
    return 0
  fi
  if ! mkdir -p -- "$dir" 2>/dev/null; then
    warn "无法创建配置目录: $dir"
    return 1
  fi
  chmod 700 -- "$dir" 2>/dev/null || true
  return 0
}

# 读取配置，只填充当前尚未由环境变量/命令行指定的非密钥字段。
load_config() {
  local line key value normalized perm found=0
  [ -n "${CONFIG_FILE:-}" ] || return 0
  [ -f "$CONFIG_FILE" ] || return 0

  if command -v stat >/dev/null 2>&1; then
    perm="$(stat -c '%a' "$CONFIG_FILE" 2>/dev/null || true)"
    case "$perm" in
      400|600|'') ;;
      *) warn "配置文件权限为 ${perm}，建议执行: chmod 600 $CONFIG_FILE" ;;
    esac
  fi

  while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in
      ''|'#'*) continue ;;
    esac
    key="${line%%=*}"
    [ "$key" != "$line" ] || continue
    value="$(trim "${line#*=}")"
    case "$key" in
      APB_SERVER)
        if [ -z "$SERVER" ]; then SERVER="$value"; found=1; fi
        ;;
      APB_NAME)
        if [ -z "$NAME" ]; then NAME="$value"; found=1; fi
        ;;
      APB_CHANNEL)
        if [ "$CHANNEL_EXPLICIT" -eq 0 ] && [ -n "$value" ]; then
          if normalized="$(normalize_channel "$value")"; then
            CHANNEL="$normalized"
            CHANNEL_EXPLICIT=1
            found=1
          else
            warn "配置文件中的 APB_CHANNEL 无效: $value"
          fi
        fi
        ;;
      APB_BACKGROUND)
        if [ "$BACKGROUND_MODE" = "ask" ] && [ -n "$value" ]; then
          BACKGROUND_MODE="$value"
          found=1
        fi
        ;;
      APB_INSTALL_DIR)
        if [ "$INSTALL_DIR_EXPLICIT" -eq 0 ] && [ -z "${APB_INSTALL_DIR:-}" ] && [ -n "$value" ]; then
          DEFAULT_INSTALL_DIR="$value"
          found=1
        fi
        ;;
    esac
  done < "$CONFIG_FILE"

  if [ "$found" -eq 1 ]; then
    CONFIG_LOADED=1
    info "已加载配置: $CONFIG_FILE"
  fi
  return 0
}

# 写入非密钥配置；APB_KEY 永远不落盘。
save_config() {
  local tmp
  ensure_config_dir || return 1
  tmp="${CONFIG_FILE}.tmp.$$"
  if (
    umask 077
    {
      printf '%s\n' '# apb 客户端管理配置（不包含 APB_KEY）'
      if [ -n "$SERVER" ]; then
        printf 'APB_SERVER=%s\n' "$SERVER"
      fi
      if [ -n "$NAME" ]; then
        printf 'APB_NAME=%s\n' "$NAME"
      fi
      if [ -n "$CHANNEL" ]; then
        printf 'APB_CHANNEL=%s\n' "$CHANNEL"
      fi
      case "$BACKGROUND_MODE" in
        1|0) printf 'APB_BACKGROUND=%s\n' "$BACKGROUND_MODE" ;;
      esac
      if [ -n "$DEFAULT_INSTALL_DIR" ]; then
        printf 'APB_INSTALL_DIR=%s\n' "$DEFAULT_INSTALL_DIR"
      fi
    } > "$tmp"
  ); then
    if mv -f -- "$tmp" "$CONFIG_FILE" 2>/dev/null; then
      chmod 600 -- "$CONFIG_FILE" 2>/dev/null || true
      return 0
    fi
  fi
  rm -f -- "$tmp" 2>/dev/null || true
  warn "无法写入配置文件: $CONFIG_FILE"
  return 1
}

persist_config() {
  [ "$PERSIST_CONFIG" -eq 1 ] || return 0
  if save_config; then
    info "配置已保存: $CONFIG_FILE（APB_KEY 未写入）"
    CONFIG_LOADED=1
  else
    warn "配置保存失败，将继续执行"
  fi
  return 0
}

require_tty() {
  [ "$INPUT_FROM_TTY" -ne 0 ] || die "当前没有可用的终端输入，无法进行交互配置"
}

edit_server() {
  local ans default="$SERVER"
  require_tty
  while :; do
    if [ -n "$default" ]; then
      prompt_read ans "服务端地址（回车保持 ${default}）: " 0 || die "没有可用的终端输入"
    else
      prompt_read ans "服务端地址（IP:端口，可省略端口，默认 ${DEFAULT_PORT}）: " 0 || die "没有可用的终端输入"
    fi
    ans="$(trim "$ans")"
    if [ -z "$ans" ]; then
      if [ -z "$default" ]; then
        warn "服务端地址不能为空。"
        continue
      fi
      ans="$default"
    fi
    if normalize_server "$ans"; then
      SERVER="$SERVER_NORM"
      return 0
    fi
    warn "服务端地址格式不正确: $ans"
    default=""
  done
}

edit_name() {
  local ans default="$NAME" fallback
  require_tty
  fallback="$(default_name)"
  [ -n "$default" ] || default="$fallback"
  while :; do
    prompt_read ans "节点名称（回车保持 ${default}）: " 0 || die "没有可用的终端输入"
    ans="$(trim "$ans")"
    [ -n "$ans" ] || ans="$default"
    if validate_name "$ans"; then
      NAME="$ans"
      return 0
    fi
    warn "节点名格式不正确: $ans"
    default="$fallback"
  done
}

edit_key() {
  local ans="" prompt=""
  require_tty
  if [ -n "$KEY" ]; then
    prompt="APB_KEY（回车保持本次会话已输入的密钥，输入新值则更新；不落盘）: "
  else
    prompt="APB_KEY（64 位 hex，输入不回显；可留空稍后启动时输入）: "
  fi
  while :; do
    prompt_read ans "$prompt" 1 || die "没有可用的终端输入"
    ans="$(trim "$ans")"
    if [ -z "$ans" ]; then
      return 0
    fi
    if validate_key "$ans"; then
      KEY="$ans"
      return 0
    fi
    warn "APB_KEY 格式不正确"
    prompt="APB_KEY（请重新输入 64 位 hex，或留空保持原值）: "
  done
}

edit_channel() {
  local ans default=1
  require_tty
  [ "$CHANNEL" = "prerelease" ] && default=2
  while :; do
    prompt_read ans "下载通道（1 正式版，2 预发布，回车保持 $(channel_name)）: " 0 || die "没有可用的终端输入"
    ans="$(trim "$ans")"
    case "${ans:-$default}" in
      1|stable|release|正式版|正式) CHANNEL="stable"; CHANNEL_EXPLICIT=1; return 0 ;;
      2|pre|prerelease|pre-release|preview|beta|预发布|预发布版) CHANNEL="prerelease"; CHANNEL_EXPLICIT=1; return 0 ;;
      *) warn "输入无效: ${ans}（请输入 1 或 2）" ;;
    esac
  done
}

edit_background() {
  local ans default=1 label="后台"
  require_tty
  if [ "$BACKGROUND_MODE" = "0" ]; then
    default=2
    label="前台"
  fi
  while :; do
    prompt_read ans "运行方式（1 后台，2 前台，回车保持 ${label}）: " 0 || die "没有可用的终端输入"
    ans="$(trim "$ans")"
    case "${ans:-$default}" in
      1|b|B|background|后台) BACKGROUND_MODE=1; return 0 ;;
      2|f|F|foreground|前台) BACKGROUND_MODE=0; return 0 ;;
      *) warn "输入无效: ${ans}（请输入 1 或 2）" ;;
    esac
  done
}

action_config() {
  require_tty
  printf '\n'
  info "修改连接配置（APB_KEY 不落盘，可只改其他字段）"
  edit_server
  edit_name
  edit_key
  edit_channel
  edit_background
  PERSIST_CONFIG=1
  save_config || die "配置保存失败: $CONFIG_FILE"
  ok "配置已保存: $CONFIG_FILE"
  printf '    APB_KEY 将在启动/重启时读取，不会写入配置文件。\n'
  return 0
}

# ================= PID / 进程识别 =================

_is_zombie() {
  local pid="$1" st=""
  [ -r "/proc/$pid/status" ] || return 1
  st="$(awk '/^State:/{print $2; exit}' "/proc/$pid/status" 2>/dev/null || true)"
  [ "$st" = "Z" ]
}

_pid_uses_agent() {
  local pid="$1" arg has_agent=0
  [ -r "/proc/$pid/cmdline" ] || return 1
  while IFS= read -r arg; do
    case "$arg" in
      agent) has_agent=1; break ;;
    esac
  done < <(tr '\000' '\n' < "/proc/$pid/cmdline" 2>/dev/null || true)
  [ "$has_agent" -eq 1 ]
}

_pid_env_value() {
  local pid="$1" want="$2" line
  [ -r "/proc/$pid/environ" ] || return 1
  while IFS= read -r line; do
    case "$line" in
      "$want"=*) printf '%s\n' "${line#*=}"; return 0 ;;
    esac
  done < <(tr '\000' '\n' < "/proc/$pid/environ" 2>/dev/null || true)
  return 1
}

_exe_looks_like_apb() {
  local exe="$1"
  case "$exe" in
    */apb|*/apb\ \(deleted\)) return 0 ;;
    */apb-*|*/apb.*) return 0 ;;
  esac
  if [ -n "${APB_BIN_PATH:-}" ]; then
    case "$exe" in
      "$APB_BIN_PATH"|"$APB_BIN_PATH (deleted)") return 0 ;;
    esac
  fi
  return 1
}

# 判定 PID 文件里的进程是否由本工具管理（信任 PID 文件，不要求节点名仍然和配置一致）。
_pid_is_managed_agent() {
  local pid="$1" exe="" env_server="" env_name=""
  case "$pid" in
    ''|*[!0-9]*) return 1 ;;
  esac
  [ -d "/proc/$pid" ] || return 1
  _is_zombie "$pid" && return 1
  _pid_uses_agent "$pid" || return 1

  exe="$(readlink "/proc/$pid/exe" 2>/dev/null || true)"
  if _exe_looks_like_apb "$exe"; then
    return 0
  fi
  if [ -n "${APB_BIN_PATH:-}" ]; then
    case "$exe" in
      "$APB_BIN_PATH"|"$APB_BIN_PATH (deleted)") return 0 ;;
    esac
  fi
  if [ -n "${BIN_OVERRIDE:-}" ]; then
    case "$exe" in
      "$BIN_OVERRIDE"|"$BIN_OVERRIDE (deleted)") return 0 ;;
    esac
  fi
  env_server="$(_pid_env_value "$pid" APB_SERVER 2>/dev/null || true)"
  if [ -n "$env_server" ]; then
    return 0
  fi
  env_name="$(_pid_env_value "$pid" APB_NAME 2>/dev/null || true)"
  [ -n "$env_name" ] || return 1
  return 0
}

# 严格判定进程是否匹配当前配置（用于 PID 文件缺失时从 /proc 扫描）。
_pid_matches_agent() {
  local pid="$1" exe="" env_name="" env_server=""
  case "$pid" in
    ''|*[!0-9]*) return 1 ;;
  esac
  [ -d "/proc/$pid" ] || return 1
  _is_zombie "$pid" && return 1
  _pid_uses_agent "$pid" || return 1

  if [ -n "${NAME:-}" ]; then
    env_name="$(_pid_env_value "$pid" APB_NAME 2>/dev/null || true)"
    if [ -n "$env_name" ]; then
      [ "$env_name" = "$NAME" ]
      return
    fi
    if [ -n "${SERVER:-}" ]; then
      env_server="$(_pid_env_value "$pid" APB_SERVER 2>/dev/null || true)"
      if [ -n "$env_server" ]; then
        [ "$env_server" = "$SERVER" ]
        return
      fi
    fi
    return 1
  fi

  exe="$(readlink "/proc/$pid/exe" 2>/dev/null || true)"
  if _exe_looks_like_apb "$exe"; then
    return 0
  fi
  if [ -n "${APB_BIN_PATH:-}" ]; then
    case "$exe" in
      "$APB_BIN_PATH"|"$APB_BIN_PATH (deleted)") return 0 ;;
    esac
  fi
  return 1
}

_write_pid_file() {
  local pid="$1" tmp
  [ -n "$pid" ] || return 1
  ensure_config_dir >/dev/null 2>&1 || return 1
  tmp="${PID_FILE}.tmp.$$"
  if printf '%s\n' "$pid" > "$tmp" 2>/dev/null && mv -f -- "$tmp" "$PID_FILE" 2>/dev/null; then
    return 0
  fi
  rm -f -- "$tmp" 2>/dev/null || true
  return 1
}

get_running_pid() {
  local pid="" link tmp
  if [ -r "$PID_FILE" ]; then
    IFS= read -r pid < "$PID_FILE" || pid=""
    if [ -n "$pid" ] && _pid_is_managed_agent "$pid"; then
      printf '%s\n' "$pid"
      return 0
    fi
    rm -f -- "$PID_FILE" 2>/dev/null || true
  fi

  # PID 文件缺失（例如旧版脚本启动）时，从 /proc 中按 apb agent 识别。
  for link in /proc/[0-9]*/exe; do
    [ -L "$link" ] || continue
    tmp="${link#/proc/}"
    pid="${tmp%/exe}"
    if [ "$pid" = "$$" ]; then
      continue
    fi
    if _pid_matches_agent "$pid"; then
      _write_pid_file "$pid" >/dev/null 2>&1 || true
      printf '%s\n' "$pid"
      return 0
    fi
  done
  return 1
}

stop_agent() {
  local pid="" waited=0
  pid="$(get_running_pid || true)"
  if [ -z "$pid" ]; then
    rm -f -- "$PID_FILE" 2>/dev/null || true
    info "没有活跃的 apb agent 进程"
    return 0
  fi

  info "正在停止 apb agent (PID: $pid)..."
  kill -TERM "$pid" 2>/dev/null || true
  while _pid_is_managed_agent "$pid" && [ "$waited" -lt 10 ]; do
    sleep 1
    waited=$((waited + 1))
  done

  if _pid_is_managed_agent "$pid"; then
    warn "PID $pid 仍未退出，发送 SIGKILL ..."
    kill -KILL "$pid" 2>/dev/null || true
    sleep 1
  fi
  if _pid_is_managed_agent "$pid"; then
    error "无法停止 apb agent (PID: $pid)"
    return 1
  fi

  rm -f -- "$PID_FILE" 2>/dev/null || true
  ok "apb agent 已停止"
  return 0
}

# ================= 启动 / 更新 / 状态 =================

resolve_log_file() {
  local safe_name log_dir
  if [ -n "${APB_LOG:-}" ]; then
    LOG_FILE="$APB_LOG"
    return 0
  fi
  safe_name="$(sanitize_log_name "${NAME:-node}")"
  [ -n "$safe_name" ] || safe_name="node"
  LOG_FILE=""
  for log_dir in "${TMPDIR:-/tmp}" "${HOME:-}" "."; do
    [ -n "$log_dir" ] || continue
    if [ -d "$log_dir" ] && [ -w "$log_dir" ]; then
      LOG_FILE="${log_dir}/apb-agent-${safe_name}.log"
      return 0
    fi
  done
  return 1
}

check_platform() {
  local os
  os="$(uname -s 2>/dev/null || true)"
  [ "$os" = "Linux" ] || die "本脚本只支持 Linux，当前系统: ${os:-unknown}"
}

prepare_agent_binary() {
  local target
  check_platform
  target="$(detect_target)"
  ask_channel
  resolve_binary "$target"
  [ -n "$APB_BIN_PATH" ] || die "无法准备 apb 二进制"
  APB_BIN_VERSION="${APB_BIN_VERSION:-$("$APB_BIN_PATH" --version 2>/dev/null || printf 'apb')}"
  info "使用二进制: ${APB_BIN_PATH} (${APB_BIN_VERSION})"
  return 0
}

show_config_summary() {
  local run_mode="后台"
  if [ "$BACKGROUND_MODE" = "0" ]; then
    run_mode="前台"
  fi
  printf '\n'
  info "配置确认"
  printf '    服务端   : %s\n' "$SERVER"
  printf '    节点名   : %s\n' "$NAME"
  printf '    密钥     : 已输入（不显示、不落盘）\n'
  printf '    下载通道 : %s\n' "$(channel_name)"
  printf '    运行方式 : %s\n' "$run_mode"
  printf '\n'
}

action_stop() {
  stop_agent
}

action_start() {
  prepare_agent_binary
  ask_server
  ask_key
  ask_name
  ask_background
  persist_config
  show_config_summary
  start_agent
}

action_restart() {
  prepare_agent_binary
  ask_server
  ask_key
  ask_name
  ask_background
  persist_config
  show_config_summary
  stop_agent || return 1
  sleep 1
  start_agent
}

action_update() {
  local running=""
  check_platform
  running="$(get_running_pid || true)"
  FORCE_UPDATE=1
  prepare_agent_binary
  persist_config
  ok "apb 二进制已更新: ${APB_BIN_PATH} (${APB_BIN_VERSION})"
  if [ -n "$running" ]; then
    info "agent 正在运行 (PID: $running)，新二进制将在下次重启时生效"
    info "可返回菜单选择 3. 重启 apb agent"
  fi
  return 0
}

action_status() {
  local pid="" candidate="" ps_out="" run_mode="后台"
  local -a candidates=()
  pid="$(get_running_pid || true)"
  if [ -n "$pid" ]; then
    ok "apb agent 正在运行 (PID: $pid)"
    if command -v ps >/dev/null 2>&1; then
      ps_out="$(ps -o pid=,etime=,cmd= -p "$pid" 2>/dev/null || true)"
      [ -n "$ps_out" ] && printf '    %s\n' "$ps_out"
    fi
  else
    warn "apb agent 当前未运行"
  fi

  printf '\n'
  if [ "$CONFIG_LOADED" -eq 1 ]; then
    info "配置文件: ${CONFIG_FILE}"
  else
    info "配置文件: ${CONFIG_FILE}（尚未创建/未加载）"
  fi
  printf '    服务端   : %s\n' "${SERVER:-未配置}"
  printf '    节点名   : %s\n' "${NAME:-未配置}"
  printf '    下载通道 : %s\n' "$(channel_name)"

  if [ "$BACKGROUND_MODE" = "0" ]; then
    run_mode="前台"
  elif [ "$BACKGROUND_MODE" = "1" ]; then
    run_mode="后台"
  else
    run_mode="默认后台"
  fi
  printf '    运行方式 : %s\n' "$run_mode"

  if [ -n "$KEY" ]; then
    printf '    APB_KEY  : 已加载（本次会话，不显示）\n'
  else
    printf '    APB_KEY  : 未加载（启动/重启时输入）\n'
  fi

  [ -n "${APB_BIN_PATH:-}" ] && candidates+=("$APB_BIN_PATH")
  [ -n "${BIN_OVERRIDE:-}" ] && candidates+=("$BIN_OVERRIDE")
  candidates+=("${DEFAULT_INSTALL_DIR%/}/apb")
  if [ -n "${SCRIPT_DIR:-}" ]; then
    candidates+=("${SCRIPT_DIR}/apb")
  fi
  for candidate in "${candidates[@]}"; do
    [ -n "$candidate" ] || continue
    if validate_binary "$candidate"; then
      info "已安装二进制: ${candidate} (${APB_BIN_VERSION})"
      break
    fi
  done

  if resolve_log_file; then
    if [ -f "$LOG_FILE" ]; then
      printf '\n'
      info "最近日志 (${LOG_FILE}):"
      tail -n 15 -- "$LOG_FILE" 2>/dev/null || true
    else
      printf '    日志文件 : %s（暂无）\n' "$LOG_FILE"
    fi
  fi
  return 0
}

legacy_start() {
  check_platform
  prepare_agent_binary
  if [ "$INSTALL_ONLY" -eq 1 ]; then
    ok "apb 二进制已就绪，未启动 agent。"
    return 0
  fi
  ask_server
  ask_key
  ask_name
  ask_background
  show_config_summary
  start_agent
}

# ================= 数字菜单 =================

show_menu() {
  local pid="" status_line=""
  if [ -t 1 ]; then
    printf '\033c'
  fi
  pid="$(get_running_pid || true)"
  if [ -n "$pid" ]; then
    status_line="● 运行中 (PID: $pid)"
  else
    status_line="✗ 已停止"
  fi
  cat <<EOF
=====================================
          apb 客户端管理脚本
=====================================
  配置文件: ${CONFIG_FILE}
  服务端  : ${SERVER:-未配置}
  节点名  : ${NAME:-未配置}
  当前状态: ${status_line}
-------------------------------------
  1. 启动 apb agent
  2. 停止 apb agent
  3. 重启 apb agent
  4. 更新 apb 二进制
  5. 查看运行状态
  6. 修改连接配置
  0. 退出脚本
=====================================
EOF
}

pause_return() {
  local _
  printf '\n'
  prompt_read _ "按回车键返回主菜单..." 0 || return 1
  return 0
}

interactive_menu() {
  local choice="" rc=0
  if [ "$INPUT_FROM_TTY" -eq 0 ]; then
    die "当前没有可用的终端，无法显示菜单（可使用 start/stop/restart/update/status 命令）"
  fi

  PERSIST_CONFIG=1
  while :; do
    show_menu
    if ! prompt_read choice "请输入选项 [0-6]: " 0; then
      printf '\n' >&2
      return 0
    fi
    choice="$(trim "$choice")"
    printf '\n'
    case "$choice" in
      1|start)
        if action_start; then rc=0; else rc=$?; warn "启动操作失败（退出码 $rc）"; fi
        ;;
      2|stop)
        if stop_agent; then rc=0; else rc=$?; warn "停止操作失败（退出码 $rc）"; fi
        ;;
      3|restart)
        if action_restart; then rc=0; else rc=$?; warn "重启操作失败（退出码 $rc）"; fi
        ;;
      4|update)
        if action_update; then rc=0; else rc=$?; warn "更新操作失败（退出码 $rc）"; fi
        ;;
      5|status)
        if action_status; then rc=0; else rc=$?; warn "状态查询失败（退出码 $rc）"; fi
        ;;
      6|config)
        if action_config; then rc=0; else rc=$?; warn "配置修改失败（退出码 $rc）"; fi
        ;;
      0|exit|quit|q)
        ok "退出脚本。"
        return 0
        ;;
      *)
        warn "无效选项: ${choice:-空}（请输入 0-6）"
        ;;
    esac
    pause_return || return 0
  done
}

# shellcheck disable=SC2030,SC2031
start_agent() {
  local pid="" running=""
  running="$(get_running_pid || true)"
  if [ -n "$running" ]; then
    warn "apb agent 已在运行 (PID: $running)，无需重复启动"
    return 0
  fi

  if ! resolve_log_file; then
    die "找不到可写的后台日志目录"
  fi

  if [ "$BACKGROUND_MODE" -eq 1 ]; then
    local log_parent
    log_parent="$(dirname -- "$LOG_FILE")"
    mkdir -p -- "$log_parent" 2>/dev/null || true
    : >>"$LOG_FILE" 2>/dev/null || die "无法写入后台日志: $LOG_FILE"

    info "后台启动 apb agent ..."
    # 在子 shell 里 export，避免把密钥放进临时 `env VAR=...` 命令参数。
    (
      export APB_SERVER="$SERVER" APB_KEY="$KEY" APB_NAME="$NAME"
      if command -v nohup >/dev/null 2>&1; then
        exec nohup "$APB_BIN_PATH" agent
      elif command -v setsid >/dev/null 2>&1; then
        exec setsid "$APB_BIN_PATH" agent
      else
        exec "$APB_BIN_PATH" agent
      fi
    ) >>"$LOG_FILE" 2>&1 < /dev/null &
    pid=$!
    sleep 1

    if kill -0 "$pid" 2>/dev/null; then
      if ! _write_pid_file "$pid"; then
        warn "无法写入 PID 文件: $PID_FILE"
      fi
      ok "apb agent 已在后台运行"
      printf '    节点名   : %s\n' "$NAME"
      printf '    服务端   : %s\n' "$SERVER"
      printf '    进程 PID : %s\n' "$pid"
      printf '    日志     : %s\n' "$LOG_FILE"
      printf '    停止方式 : 运行本脚本，选择 2. 停止 apb agent\n'
      return 0
    fi

    warn "apb agent 启动后立即退出，最近日志如下："
    tail -n 20 -- "$LOG_FILE" >&2 2>/dev/null || true
    return 1
  fi

  info "前台运行 apb agent（Ctrl+C 停止）..."
  printf '    节点名 : %s\n' "$NAME"
  printf '    服务端 : %s\n' "$SERVER"
  export APB_SERVER="$SERVER" APB_KEY="$KEY" APB_NAME="$NAME"
  exec "$APB_BIN_PATH" agent
}

main() {
  local action="$ACTION" original_argc=$#

  # 支持 1/2/3 数字命令，以及 start / stop / restart / update / status / config。
  if [ "$#" -gt 0 ]; then
    case "$1" in
      menu)        action="menu"; shift ;;
      0)           action="menu"; shift ;;
      1|start|run) action="start"; shift ;;
      2|stop)      action="stop"; shift ;;
      3|restart)   action="restart"; shift ;;
      4|update)    action="update"; shift ;;
      5|status)    action="status"; shift ;;
      6|config|configure) action="config"; shift ;;
    esac
  fi

  parse_args "$@"
  if [ -n "$ACTION" ]; then
    action="$ACTION"
  fi
  load_config

  # 无参数 + 有终端 + 未强制 --yes：进入数字菜单；否则保持原一次性启动行为。
  if [ -z "$action" ]; then
    if [ "$original_argc" -eq 0 ] && [ "$INPUT_FROM_TTY" -ne 0 ] && [ "$ASSUME_YES" -ne 1 ]; then
      action="menu"
    else
      action="legacy"
    fi
  fi

  case "$action" in
    menu)    PERSIST_CONFIG=1; interactive_menu ;;
    start)   PERSIST_CONFIG=1; action_start ;;
    stop)    action_stop ;;
    restart) PERSIST_CONFIG=1; action_restart ;;
    update)  PERSIST_CONFIG=1; action_update ;;
    status)  action_status ;;
    config)  PERSIST_CONFIG=1; action_config ;;
    legacy)  PERSIST_CONFIG=0; legacy_start ;;
    *)       die "未知命令: $action" ;;
  esac
}

main "$@"
