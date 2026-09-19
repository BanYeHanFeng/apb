#!/usr/bin/env bash
#
# apb 客户端综合管理脚本（纯数字菜单）：
#   启动 / 停止 / 重启 agent、安装 / 更新 apb 二进制、查看状态、修改连接配置。
#
# 用法（仅支持在交互式终端中无参数运行；不接受任何命令行选项）：
#   bash <(curl -fsSL https://raw.githubusercontent.com/BanYeHanFeng/apb/main/runApb.sh)
#   无参数运行会显示数字菜单，所有操作都在菜单中完成。
#
# 环境变量（只影响对应菜单项的默认值，不提供绕过菜单的一次性执行入口）：
#   APB_SERVER / APB_KEY / APB_NAME   服务端地址、密钥、节点名
#   APB_CONFIG                        管理配置文件，默认 ~/.config/apb/agent.conf
#   APB_CONFIG_DIR                    管理配置目录，默认 ~/.config/apb
#   APB_PID_FILE                      agent PID 文件，默认 <配置目录>/agent.pid
#   APB_INSTALL_DIR                   二进制安装目录，默认 ~/.local/bin
#   APB_BIN                           已存在的 apb 二进制，安装菜单中复用
#   APB_BINARY_URL                    直接指定二进制下载地址
#   APB_REPO                          GitHub 仓库，默认 BanYeHanFeng/apb
#   APB_CHANNEL                       安装/更新默认通道，stable（默认）/ prerelease
#   APB_PRE_RELEASE_TAG               预发布 Release 标签，默认 pre-release
#   APB_VERSION                       Release 版本，默认 latest
#   APB_RELEASE_BASE                  Release 下载页，默认 https://github.com/$APB_REPO/releases
#   APB_GH_PROXY                      GitHub 加速前缀，按 <前缀>/<完整URL> 拼装
#   APB_LOG                           后台日志路径，默认可写的临时目录或 HOME 下 apb-agent-<name>.log
#   APB_BACKGROUND=1                  启动时默认后台运行；0 默认前台
#   APB_TARGET                        覆盖自动检测的 Rust target / 资产后缀
#
# 说明：
#   脚本会把服务端地址、密钥、节点名、下载通道、运行方式等配置保存到 APB_CONFIG
#   （权限 600）；启动/重启时不再通过环境变量传参，而是让 agent 通过
#   `--config <文件>` 读取。apb 二进制本身仍保留读取 APB_* 环境变量。
#
set -Eeuo pipefail

DEFAULT_REPO="${APB_REPO:-BanYeHanFeng/apb}"
DEFAULT_INSTALL_DIR="${APB_INSTALL_DIR:-${HOME:-.}/.local/bin}"
DEFAULT_RELEASE_BASE="${APB_RELEASE_BASE:-https://github.com/${DEFAULT_REPO}/releases}"
BACKGROUND_MODE="${APB_BACKGROUND:-ask}"   # ask / 1 / 0
FORCE_UPDATE=0
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

# 纯数字菜单模式。
PERSIST_CONFIG=0
CONFIG_LOADED=0

# 只允许从终端读取输入；没有终端时无法显示数字菜单。
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
    printf '预发布版'
  else
    printf '正式版'
  fi
}


normalize_env_defaults() {
  case "$BACKGROUND_MODE" in
    1|true|TRUE|yes|YES) BACKGROUND_MODE=1 ;;
    0|false|FALSE|no|NO) BACKGROUND_MODE=0 ;;
    ask|"") BACKGROUND_MODE=ask ;;
    *) die "APB_BACKGROUND 值无效: $BACKGROUND_MODE" ;;
  esac

  local normalized_channel
  if ! normalized_channel="$(normalize_channel "$CHANNEL")"; then
    die "APB_CHANNEL 无效: ${CHANNEL}（可选 stable 正式版 / prerelease 预发布版）"
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
  local raw host port p
  raw="$(trim "$1")"
  [ -n "$raw" ] || return 1
  [[ "$raw" != *[[:space:]]* ]] || return 1
  case "$raw" in
    *://*) return 1 ;;
  esac

  if [[ "$raw" =~ ^\[([0-9A-Fa-f:.]+)\]:([0-9]{1,5})$ ]]; then
    host="${BASH_REMATCH[1]}"; port="${BASH_REMATCH[2]}"
  elif [[ "$raw" =~ ^([A-Za-z0-9._-]+):([0-9]{1,5})$ ]]; then
    host="${BASH_REMATCH[1]}"; port="${BASH_REMATCH[2]}"
  else
    # 不固定默认端口：server 地址必须显式带端口。
    return 1
  fi

  p=$((10#$port))
  if [ "$p" -lt 1 ] || [ "$p" -gt 65535 ]; then
    return 1
  fi
  if [[ "$host" == *:* ]]; then
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
  while :; do
    prompt_read ans "请输入下载通道（1=正式版 latest，2=预发布版 pre-release，回车默认 1）: " 0 \
      || die "没有可用的终端输入，无法询问下载通道"
    ans="$(trim "$ans")"
    case "${ans:-1}" in
      1|stable|release|正式版|正式)
        CHANNEL="stable"
        return 0
        ;;
      2|pre|pre-release|prerelease|preview|beta|预发布版|预发布|预览版|预览)
        CHANNEL="prerelease"
        # 用户主动选择预发布版时跳过本地 / 旧缓存二进制，确保从预发布 Release 下载。
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
    if [ -n "$SERVER" ]; then
      if normalize_server "$SERVER"; then
        SERVER="$SERVER_NORM"
        return 0
      fi
      if [ "$INPUT_FROM_TTY" -eq 0 ]; then
        die "APB_SERVER 格式非法（必须包含端口）: $SERVER"
      fi
      warn "已有 APB_SERVER 格式非法（必须包含端口）: $SERVER"
      SERVER=""
    fi
    if [ "$INPUT_FROM_TTY" -eq 0 ]; then
      die "没有可用的终端输入，无法询问 APB_SERVER"
    fi
    prompt_read ans "请输入服务端地址（IP:端口，必须包含端口，例如 1.2.3.4:30021）: " 0 \
      || die "没有可用的终端输入，无法询问 APB_SERVER"
    ans="$(trim "$ans")"
    if [ -z "$ans" ]; then
      warn "服务端地址不能为空。"
      continue
    fi
    if ! normalize_server "$ans"; then
      warn "服务端地址格式不正确（必须包含端口）: $ans"
      continue
    fi
    SERVER="$SERVER_NORM"
    return 0
  done
}

ask_key() {
  local ans
  while :; do
    if [ -n "$KEY" ]; then
      if validate_key "$KEY"; then
        return 0
      fi
      if [ "$INPUT_FROM_TTY" -eq 0 ]; then
        die "APB_KEY 格式非法"
      fi
      warn "已有 APB_KEY 格式非法"
      KEY=""
    fi
    if [ "$INPUT_FROM_TTY" -eq 0 ]; then
      die "没有可用的终端输入，无法询问 APB_KEY"
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
    if [ -n "$NAME" ]; then
      NAME="$(trim "$NAME")"
      if validate_name "$NAME"; then
        return 0
      fi
      if [ "$INPUT_FROM_TTY" -eq 0 ]; then
        die "APB_NAME 格式非法: $NAME"
      fi
      warn "已有 APB_NAME 格式非法: $NAME"
      NAME=""
    fi
    if [ "$INPUT_FROM_TTY" -eq 0 ]; then
      die "没有可用的终端输入，无法询问 APB_NAME"
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

  while :; do
    prompt_read ans "请输入运行方式（1 后台运行，2 前台运行，回车默认 1）: " 0 \
      || die "没有可用的终端输入，无法询问运行方式"
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

# 读取配置，只填充当前尚未由环境变量指定的非密钥字段。
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
    case "$line" in
      *=*) ;;
      *) continue ;;
    esac
    key="$(trim "${line%%=*}")"
    [ -n "$key" ] || continue
    value="$(trim "${line#*=}")"
    case "$key" in
      APB_SERVER)
        if [ -z "$SERVER" ]; then SERVER="$value"; found=1; fi
        ;;
      APB_KEY)
        if [ -z "$KEY" ] && [ -n "$value" ]; then KEY="$value"; found=1; fi
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
        if [ -z "${APB_INSTALL_DIR:-}" ] && [ -n "$value" ]; then
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

# 写入完整配置；APB_KEY 与其它字段一样保存在 600 权限的配置文件里。
save_config() {
  local tmp
  ensure_config_dir || return 1
  tmp="${CONFIG_FILE}.tmp.$$"
  if (
    umask 077
    {
      printf '%s\n' '# apb 客户端管理配置（包含 APB_KEY，请保持 600 权限）'
      if [ -n "$SERVER" ]; then
        printf 'APB_SERVER=%s\n' "$SERVER"
      fi
      if [ -n "$KEY" ]; then
        printf 'APB_KEY=%s\n' "$KEY"
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
    info "配置已保存: $CONFIG_FILE（密钥已写入，权限 600）"
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
      prompt_read ans "服务端地址（IP:端口，回车保持 ${default}）: " 0 || die "没有可用的终端输入"
    else
      prompt_read ans "服务端地址（IP:端口，必须包含端口）: " 0 || die "没有可用的终端输入"
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
    warn "服务端地址格式不正确（必须包含端口）: $ans"
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
  if [ -n "$KEY" ] && ! validate_key "$KEY"; then
    warn "配置中的 APB_KEY 格式不正确，请重新输入"
    KEY=""
  fi
  if [ -n "$KEY" ]; then
    prompt="APB_KEY（回车保持已保存的密钥，输入新值则更新；不显示）: "
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
    prompt_read ans "下载通道（1 正式版，2 预发布版，回车保持 $(channel_name)）: " 0 || die "没有可用的终端输入"
    ans="$(trim "$ans")"
    case "${ans:-$default}" in
      1|stable|release|正式版|正式) CHANNEL="stable"; CHANNEL_EXPLICIT=1; return 0 ;;
      2|pre|prerelease|pre-release|preview|beta|预发布版|预发布) CHANNEL="prerelease"; CHANNEL_EXPLICIT=1; return 0 ;;
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
  info "修改连接配置（密钥会保存到配置文件，权限 600）"
  edit_server
  edit_name
  edit_key
  edit_channel
  edit_background
  PERSIST_CONFIG=1
  save_config || die "配置保存失败: $CONFIG_FILE"
  ok "配置已保存: $CONFIG_FILE"
  printf '    APB_KEY 已写入配置文件，不会通过环境变量传给 agent。\n'
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

# 读取 /proc/<pid>/cmdline 中某个选项的值（支持 `--opt value` 与 `--opt=value`）。
_pid_arg_value() {
  local pid="$1" want="$2" prev="" arg
  [ -r "/proc/$pid/cmdline" ] || return 1
  while IFS= read -r arg; do
    if [ "$prev" = "$want" ]; then
      printf '%s\n' "$arg"
      return 0
    fi
    case "$arg" in
      "$want"=*) printf '%s\n' "${arg#*=}"; return 0 ;;
    esac
    prev="$arg"
  done < <(tr '\000' '\n' < "/proc/$pid/cmdline" 2>/dev/null || true)
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
  local pid="$1" exe="" env_server="" env_name="" cfg=""
  case "$pid" in
    ''|*[!0-9]*) return 1 ;;
  esac
  [ -d "/proc/$pid" ] || return 1
  _is_zombie "$pid" && return 1
  _pid_uses_agent "$pid" || return 1

  # 新版脚本通过 `agent --config <文件>` 启动，进程环境里不再有 APB_*。
  cfg="$(_pid_arg_value "$pid" --config 2>/dev/null || true)"
  if [ -n "$cfg" ] && [ "$cfg" = "$CONFIG_FILE" ]; then
    return 0
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
  local pid="$1" exe="" env_name="" env_server="" cfg=""
  case "$pid" in
    ''|*[!0-9]*) return 1 ;;
  esac
  [ -d "/proc/$pid" ] || return 1
  _is_zombie "$pid" && return 1
  _pid_uses_agent "$pid" || return 1

  # 新版脚本的启动命令里带有当前配置文件路径，优先精确匹配。
  cfg="$(_pid_arg_value "$pid" --config 2>/dev/null || true)"
  if [ -n "$cfg" ] && [ "$cfg" = "$CONFIG_FILE" ]; then
    return 0
  fi

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

  # PID 文件缺失（例如手工启动或 PID 文件被删）时，从 /proc 中按 apb agent 识别。
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

# ================= 启动 / 安装 / 更新 / 状态 =================

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

# 启动/重启只用已存在的二进制，绝不下载；没装时提示去菜单 4 安装。
resolve_installed_binary() {
  local candidate mark_file="" existing_channel=""
  APB_BIN_PATH=""
  APB_BIN_VERSION=""

  if [ -n "$BIN_OVERRIDE" ]; then
    BIN_OVERRIDE="$(cd -- "$(dirname -- "$BIN_OVERRIDE")" 2>/dev/null && pwd)/$(basename -- "$BIN_OVERRIDE")"
    if ! validate_binary "$BIN_OVERRIDE"; then
      error "指定的 APB_BIN 不可用或不是 apb 二进制: $BIN_OVERRIDE"
      return 1
    fi
    APB_BIN_PATH="$BIN_OVERRIDE"
    return 0
  fi

  local -a candidates=()
  # 默认通道下仍优先复用脚本目录 / 当前目录中的 apb，方便仓库内直接启动；
  # 显式配置了通道时只认同安装目录 / PATH 中的已安装二进制。
  if [ "$CHANNEL_EXPLICIT" -ne 1 ]; then
    [ -n "$SCRIPT_DIR" ] && candidates+=("$SCRIPT_DIR/apb")
    candidates+=("./apb" "$PWD/apb")
  fi
  [ -n "${DEFAULT_INSTALL_DIR:-}" ] && candidates+=("${DEFAULT_INSTALL_DIR%/}/apb")
  if command -v apb >/dev/null 2>&1; then
    candidates+=("$(command -v apb)")
  fi

  for candidate in "${candidates[@]}"; do
    [ -n "$candidate" ] || continue
    if validate_binary "$candidate"; then
      APB_BIN_PATH="$(cd -- "$(dirname -- "$candidate")" 2>/dev/null && pwd)/$(basename -- "$candidate")"
      if [ -n "${DEFAULT_INSTALL_DIR:-}" ] && [ "$APB_BIN_PATH" = "${DEFAULT_INSTALL_DIR%/}/apb" ]; then
        mark_file="${DEFAULT_INSTALL_DIR%/}/.apb-channel"
        if [ -f "$mark_file" ]; then
          existing_channel="$(cat -- "$mark_file" 2>/dev/null || true)"
          if [ -n "$existing_channel" ] && [ "$existing_channel" != "$CHANNEL" ]; then
            warn "已安装二进制来自 ${existing_channel} 通道，当前配置为 $(channel_name)；如需切换请先在菜单选择 4. 安装 apb"
          fi
        fi
      fi
      return 0
    fi
  done

  error "未找到已安装的 apb 二进制；请先在菜单选择 4. 安装 apb"
  return 1
}

prepare_installed_binary() {
  check_platform
  if [ -n "$BINARY_URL" ]; then
    warn "已设置 APB_BINARY_URL，但启动/重启不会自动下载，将使用已安装的 apb"
  fi
  if ! resolve_installed_binary; then
    return 1
  fi
  APB_BIN_VERSION="${APB_BIN_VERSION:-$("$APB_BIN_PATH" --version 2>/dev/null || printf 'apb')}"
  info "使用已安装二进制: ${APB_BIN_PATH} (${APB_BIN_VERSION})"
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
  printf '    密钥     : 已写入配置文件（不显示）\n'
  printf '    下载通道 : %s\n' "$(channel_name)"
  printf '    运行方式 : %s\n' "$run_mode"
  printf '\n'
}

action_stop() {
  stop_agent
}

action_start() {
  prepare_installed_binary || return 1
  ask_server
  ask_key
  ask_name
  ask_background
  persist_config
  show_config_summary
  start_agent
}

action_restart() {
  prepare_installed_binary || return 1
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
    info "可返回菜单选择 3. 重启 apb"
  fi
  return 0
}

# 安装菜单显示的版本名称，使用“正式版 / 预发布版”区分。
install_channel_name() {
  if [ "$CHANNEL" = "prerelease" ]; then
    printf '预发布版（pre-release）'
  else
    printf '正式版（latest）'
  fi
}

# 安装/切换 apb 二进制：按当前 CHANNEL 强制重新下载。
action_install_binary() {
  local running="" saved_force_update="$FORCE_UPDATE"
  check_platform
  running="$(get_running_pid || true)"
  FORCE_UPDATE=1
  prepare_agent_binary
  FORCE_UPDATE="$saved_force_update"
  persist_config
  ok "apb 二进制已安装: ${APB_BIN_PATH} (${APB_BIN_VERSION})"
  printf '    安装版本 : %s\n' "$(install_channel_name)"
  if [ -n "$running" ]; then
    info "agent 正在运行 (PID: $running)，新二进制将在下次重启时生效"
    info "可返回主菜单选择 3. 重启 apb"
  fi
  return 0
}

# 菜单项 4：选择正式版 / 预发布版后调用 action_install_binary。
action_install_menu() {
  local choice=""
  require_tty
  printf '\n'
  info "安装 apb"
  printf '  1. 正式版（latest Release）\n'
  printf '  2. 预发布版（pre-release 滚动构建）\n'
  printf '  0. 返回主菜单\n'
  if [ "$VERSION" != "latest" ]; then
    warn "APB_VERSION=${VERSION} 已指定，将改为安装所选通道的 latest 版本"
  fi
  if [ -n "$BINARY_URL" ]; then
    warn "APB_BINARY_URL 已指定，将忽略版本选择并从自定义地址下载"
  fi
  if [ -n "$BIN_OVERRIDE" ]; then
    warn "APB_BIN 已指定，将复用该二进制，不下载所选版本"
  fi
  while :; do
    if ! prompt_read choice "请选择要安装的版本 [0-2]: " 0; then
      printf '\n' >&2
      return 0
    fi
    choice="$(trim "$choice")"
    case "$choice" in
      1|stable|release|latest|正式版)
        CHANNEL="stable"
        CHANNEL_EXPLICIT=1
        VERSION="latest"
        info "已选择正式版（latest Release）"
        break
        ;;
      2|pre|prerelease|pre-release|preview|beta|预发布版|预发布)
        CHANNEL="prerelease"
        CHANNEL_EXPLICIT=1
        VERSION="latest"
        info "已选择预发布版（pre-release 滚动构建）"
        break
        ;;
      0|q|exit|quit|返回)
        info "已取消安装"
        return 0
        ;;
      *)
        warn "输入无效: ${choice:-空}（请输入 0-2）"
        ;;
    esac
  done
  action_install_binary
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
    printf '    APB_KEY  : 已配置（配置文件 / 环境变量，不显示）\n'
  else
    printf '    APB_KEY  : 未配置（启动/重启时输入）\n'
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
  1. 启动 apb
  2. 停止 apb
  3. 重启 apb

  4. 安装 apb
  5. 更新 apb

  6. 查看运行状态
  7. 修改连接配置
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
    die "当前没有可用的终端，无法显示数字菜单；请在交互式终端中直接运行本脚本"
  fi

  PERSIST_CONFIG=1
  while :; do
    show_menu
    if ! prompt_read choice "请输入选项 [0-7]: " 0; then
      printf '\n' >&2
      return 0
    fi
    choice="$(trim "$choice")"
    printf '\n'
    case "$choice" in
      1)
        if action_start; then rc=0; else rc=$?; warn "启动操作失败（退出码 $rc）"; fi
        ;;
      2)
        if stop_agent; then rc=0; else rc=$?; warn "停止操作失败（退出码 $rc）"; fi
        ;;
      3)
        if action_restart; then rc=0; else rc=$?; warn "重启操作失败（退出码 $rc）"; fi
        ;;
      4)
        if action_install_menu; then rc=0; else rc=$?; warn "安装操作失败（退出码 $rc）"; fi
        ;;
      5)
        if action_update; then rc=0; else rc=$?; warn "更新操作失败（退出码 $rc）"; fi
        ;;
      6)
        if action_status; then rc=0; else rc=$?; warn "状态查询失败（退出码 $rc）"; fi
        ;;
      7)
        if action_config; then rc=0; else rc=$?; warn "配置修改失败（退出码 $rc）"; fi
        ;;
      0|exit|quit|q)
        ok "退出脚本。"
        return 0
        ;;
      *)
        warn "无效选项: ${choice:-空}（请输入 0-7）"
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
    # 配置（含密钥）已经在 600 权限的配置文件里，agent 通过 --config 读取；
    # 清掉外部残留的连接类 APB_* 环境变量，确保配置文件优先。
    (
      unset APB_SERVER APB_KEY APB_NAME APB_CONFIG APB_CONFIG_DIR
      if command -v nohup >/dev/null 2>&1; then
        exec nohup "$APB_BIN_PATH" agent --config "$CONFIG_FILE"
      elif command -v setsid >/dev/null 2>&1; then
        exec setsid "$APB_BIN_PATH" agent --config "$CONFIG_FILE"
      else
        exec "$APB_BIN_PATH" agent --config "$CONFIG_FILE"
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
      printf '    停止方式 : 运行本脚本，选择 2. 停止 apb\n'
      return 0
    fi

    warn "apb agent 启动后立即退出，最近日志如下："
    tail -n 20 -- "$LOG_FILE" >&2 2>/dev/null || true
    return 1
  fi

  info "前台运行 apb agent（Ctrl+C 停止）..."
  printf '    节点名 : %s\n' "$NAME"
  printf '    服务端 : %s\n' "$SERVER"
  # 同后台模式：不再 export 环境变量，并以显式 --config 为准。
  unset APB_SERVER APB_KEY APB_NAME APB_CONFIG APB_CONFIG_DIR
  exec "$APB_BIN_PATH" agent --config "$CONFIG_FILE"
}

main() {
  # 所有基于命令行参数的一次性执行入口均已删除，只保留交互式数字菜单。
  if [ "$#" -gt 0 ]; then
    error "本脚本不接受任何命令行参数，请直接运行后使用数字菜单。"
    exit 2
  fi
  if [ "$INPUT_FROM_TTY" -eq 0 ]; then
    die "当前没有可用的终端，无法显示数字菜单；请在交互式终端中直接运行本脚本"
  fi

  normalize_env_defaults
  load_config
  PERSIST_CONFIG=1
  interactive_menu
}

main "$@"
