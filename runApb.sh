#!/usr/bin/env bash
#
# apb 客户端一键脚本：自动选择架构、下载静态 apb，然后交互询问服务端参数并启动 agent。
#
# 推荐用法（stdin 仍留给终端，脚本读取 /dev/tty，所以可以放心交互）：
#   bash <(curl -fsSL https://raw.githubusercontent.com/BanYeHanFeng/apb/main/runApb.sh)
#
# 非交互 / CI 用法：
#   APB_SERVER=1.2.3.4:30020 APB_KEY=64位hex APB_NAME=node-a \
#     bash runApb.sh --yes --background
#
# 只安装不启动：
#   bash runApb.sh --install-only
#
# 环境变量（除了与 apb 一致的前三个，其余都只影响本脚本）：
#   APB_SERVER / APB_KEY / APB_NAME   服务端地址、密钥、节点名
#   APB_INSTALL_DIR                   二进制安装目录，默认 ~/.local/bin
#   APB_BIN                           已存在的 apb 二进制，设置后跳过下载
#   APB_BINARY_URL                    直接指定二进制下载地址
#   APB_REPO                          GitHub 仓库，默认 BanYeHanFeng/apb
#   APB_VERSION                       Release 版本，默认 latest
#   APB_RELEASE_BASE                  Release 下载页，默认 https://github.com/$APB_REPO/releases
#   APB_GH_PROXY                      GitHub 加速前缀，按 <前缀>/<完整URL> 拼装
#   APB_LOG                           后台日志路径，默认可写的临时目录或 HOME 下 apb-agent-<name>.log
#   APB_BACKGROUND=1                  等价于 --background
#   APB_YES=1                         等价于 --yes
#   APB_UPDATE=1                      等价于 --update，强制重新下载
#   APB_TARGET                        覆盖自动检测的 Rust target / 资产后缀
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
die()   { printf '[X] %s\n' "$*" >&2; exit 1; }

cleanup() {
  if [ -n "${TMP_FILE:-}" ] && [ -e "$TMP_FILE" ]; then
    rm -f -- "$TMP_FILE"
  fi
}
trap cleanup EXIT

usage() {
  cat <<EOF
apb 客户端一键脚本

用法:
  bash <(curl -fsSL https://raw.githubusercontent.com/${DEFAULT_REPO}/main/runApb.sh)
  bash runApb.sh [选项]

本脚本会:
  1. 检测 Linux 架构（x86_64 / aarch64）；
  2. 下载对应的静态 apb 到安装目录；
  3. 交互询问服务端地址、APB_KEY、节点名和运行方式；
  4. 启动 apb agent（默认后台运行）。

选项:
  -s, --server IP:PORT   服务端地址，缺省端口 ${DEFAULT_PORT}
  -k, --key KEY          64 位 hex 密钥（或 base64:...；会留在 shell 历史，建议用 APB_KEY）
  -n, --name NAME        节点名
  -b, --background       后台运行（默认）
  -f, --foreground       前台运行，Ctrl+C 停止
  -y, --yes              不交互，使用已有参数 / 默认运行方式；缺少必填项则报错
      --install-only     只下载/校验二进制，不启动 agent
      --update           强制重新下载最新二进制
      --binary PATH      使用已有 apb 二进制，不下载
      --repo OWNER/REPO  指定 GitHub 仓库，默认 ${DEFAULT_REPO}
      --release-base URL 指定 Release 地址前缀
      --install-dir DIR  指定安装目录，默认 ${DEFAULT_INSTALL_DIR}
      --target TARGET    覆盖自动检测的 Release 资产后缀
      --binary-url URL   直接指定二进制下载地址，跳过 Release 拼接
  -h, --help             显示帮助
      --version          显示本脚本版本

示例:
  bash runApb.sh
  APB_SERVER=1.2.3.4:30020 APB_KEY=\$(apb keygen) APB_NAME=phone-a \\
    bash runApb.sh --yes --background

安全提示:
  交互输入的 APB_KEY 不回显，也不会出现在 apb 进程命令行中；密钥只通过环境变量传给 agent。
  不要把密钥写进 issue、日志或公开仓库；泄露后请立即轮换服务端与所有 agent 的密钥。
EOF
}

version() {
  printf 'runApb.sh (apb client helper)\n'
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
      --binary)
        [ "$#" -ge 2 ] || die "$1 缺少参数"
        BIN_OVERRIDE="$2"; shift 2
        ;;
      --binary=*)
        BIN_OVERRIDE="${1#*=}"; shift
        ;;
      --install-dir)
        [ "$#" -ge 2 ] || die "$1 缺少参数"
        DEFAULT_INSTALL_DIR="$2"; shift 2
        ;;
      --install-dir=*)
        DEFAULT_INSTALL_DIR="${1#*=}"; shift
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
  local target="$1" candidate existing=""

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

  # 优先使用脚本目录 / 当前目录里已经构建好的 apb，方便仓库内直接运行。
  if [ "$FORCE_UPDATE" -ne 1 ]; then
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
    if [ -z "$FORCE_UPDATE" ] && validate_binary "$existing"; then
      APB_BIN_PATH="$existing"
      return 0
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
  elif [ "$VERSION" = "latest" ] || [ -z "$VERSION" ]; then
    url="${RELEASE_BASE}/latest/download/${asset}"
  else
    url="${RELEASE_BASE}/download/${VERSION}/${asset}"
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
  local target="$1" url install_dir
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
    info "检测到架构 ${target}，准备下载 ${asset}"
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
    prompt_read ans "是否后台运行？[Y/n]: " 0 || die "没有可用的终端输入，无法询问运行方式"
    ans="$(trim "$ans")"
    case "${ans:-Y}" in
      y|Y|yes|YES|Yes) BACKGROUND_MODE=1; return 0 ;;
      n|N|no|NO|No)   BACKGROUND_MODE=0; return 0 ;;
      *) warn "输入无效: $ans" ;;
    esac
  done
}

sanitize_log_name() {
  printf '%s' "$1" | tr -c 'A-Za-z0-9._-' '_'
}

# shellcheck disable=SC2030,SC2031
start_agent() {
  local safe_name log_dir
  safe_name="$(sanitize_log_name "$NAME")"
  if [ -n "${APB_LOG:-}" ]; then
    LOG_FILE="$APB_LOG"
  else
    LOG_FILE=""
    for log_dir in "${TMPDIR:-/tmp}" "${HOME:-}" "."; do
      [ -n "$log_dir" ] || continue
      if [ -d "$log_dir" ] && [ -w "$log_dir" ]; then
        LOG_FILE="${log_dir}/apb-agent-${safe_name:-node}.log"
        break
      fi
    done
    [ -n "$LOG_FILE" ] || die "找不到可写的后台日志目录"
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
    local pid=$!
    sleep 1

    if kill -0 "$pid" 2>/dev/null; then
      ok "apb agent 已在后台运行"
      printf '    节点名   : %s\n' "$NAME"
      printf '    服务端   : %s\n' "$SERVER"
      printf '    进程 PID : %s\n' "$pid"
      printf '    日志     : %s\n' "$LOG_FILE"
      printf '    停止命令 : kill %s\n' "$pid"
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
  parse_args "$@"
  local os
  os="$(uname -s 2>/dev/null || true)"
  [ "$os" = "Linux" ] || die "本脚本只支持 Linux，当前系统: ${os:-unknown}"

  local target
  target="$(detect_target)"
  resolve_binary "$target"
  [ -n "$APB_BIN_PATH" ] || die "无法准备 apb 二进制"
  APB_BIN_VERSION="${APB_BIN_VERSION:-$("$APB_BIN_PATH" --version 2>/dev/null || printf 'apb')}"
  info "使用二进制: ${APB_BIN_PATH} (${APB_BIN_VERSION})"

  if [ "$INSTALL_ONLY" -eq 1 ]; then
    ok "apb 二进制已就绪，未启动 agent。"
    return 0
  fi

  ask_server
  ask_key
  ask_name
  ask_background

  printf '\n'
  info "配置确认"
  printf '    服务端   : %s\n' "$SERVER"
  printf '    节点名   : %s\n' "$NAME"
  printf '    密钥     : 已读取（不显示）\n'
  printf '    运行方式 : %s\n' "$([ "$BACKGROUND_MODE" -eq 1 ] && printf '后台' || printf '前台')"
  printf '\n'

  start_agent
}

main "$@"
