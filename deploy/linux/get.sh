#!/usr/bin/env bash
# VOC 服务器一键安装 / 升级
#
#   curl -fsSL https://github.com/XYMYSWL/VOC/releases/latest/download/get.sh | sudo bash
#
# 没装过就全新安装；装过就升级（保留授权码和配置）。可以带 install.sh 的参数：
#   curl -fsSL .../get.sh | sudo bash -s -- --port 443 --name "我的 VOC 服"
#
# 环境变量：
#   VOC_VERSION=v0.2.1      安装指定版本（默认最新版）
#   VOC_TARBALL_URL=...     从指定地址下载安装包（仍用 GitHub 公布的 SHA-256 校验）
#
# 直连 GitHub 下载失败时自动改走国内加速镜像（ghfast.top、gh-proxy.org、gh-proxy.com）。
# 服务器连不上 GitHub 时，脚本本身也可以经镜像下载：
#   curl -fsSL https://ghfast.top/https://github.com/XYMYSWL/VOC/releases/latest/download/get.sh | sudo bash
#
# 装好后也可以直接用 `sudo voc upgrade` 升级。

set -euo pipefail

REPO="XYMYSWL/VOC"
ASSET="voc-server-linux.tar.gz"
BIN=/usr/local/bin/voc-server
CONF=/etc/voc/voc.conf

red()   { printf '\033[31m%s\033[0m\n' "$*"; }
green() { printf '\033[32m%s\033[0m\n' "$*"; }
info()  { printf '\033[36m==>\033[0m %s\n' "$*"; }
die()   { red "错误：$*"; exit 1; }

[ "$(id -u)" -eq 0 ] || die "请用 root 运行：curl -fsSL https://github.com/$REPO/releases/latest/download/get.sh | sudo bash"
command -v tar >/dev/null 2>&1 || die "需要 tar"

if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fL --retry 3 --connect-timeout 15 -sS -o "$2" "$1"; }
    fetch_text() { curl -fsSL --connect-timeout 15 "$1"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -q -O "$2" "$1"; }
    fetch_text() { wget -q -O - "$1"; }
else
    die "需要 curl 或 wget（例如：apt install curl / yum install curl）"
fi

VERSION="${VOC_VERSION:-latest}"
if [ "$VERSION" = "latest" ]; then
    URL="https://github.com/$REPO/releases/latest/download/$ASSET"
    API="https://api.github.com/repos/$REPO/releases/latest"
else
    case "$VERSION" in v*) ;; *) VERSION="v$VERSION" ;; esac
    URL="https://github.com/$REPO/releases/download/$VERSION/$ASSET"
    API="https://api.github.com/repos/$REPO/releases/tags/$VERSION"
fi
URL="${VOC_TARBALL_URL:-$URL}"

# 已安装的版本（老版本的程序不认识 --version，就显示「未知」）
OLD=""
if [ -x "$BIN" ] && [ -f "$CONF" ]; then
    OLD="$(timeout 3 "$BIN" --version 2>/dev/null | awk '/^voc-server /{print $2}' || true)"
    OLD="${OLD:-未知}"
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# 版本号和 SHA-256 来自 GitHub API（取不到时只是少一层校验，不影响安装）
META="$(fetch_text "$API" 2>/dev/null || true)"
TAG="$(printf '%s' "$META" | grep -o '"tag_name": *"[^"]*"' | head -1 | sed 's/.*"\([^"]*\)"$/\1/' || true)"
# 逐个字段扫描：记住最近的 "name"，遇到属于安装包的 "digest" 就取出来
SHA="$(printf '%s' "$META" | tr ',{}' '\n\n\n' | awk -v a="$ASSET" -F'"' '
    $2 == "name" { n = $4 }
    $2 == "digest" && n == a && $4 ~ /^sha256:/ { sub(/^sha256:/, "", $4); print $4; exit }' || true)"

info "下载 VOC 服务器 ${TAG:-$VERSION}"
MIRRORS="https://ghfast.top/ https://gh-proxy.org/ https://gh-proxy.com/"
if ! fetch "$URL" "$TMP/$ASSET"; then
    ok=0
    # 没指定下载地址时，直连失败就依次试国内加速镜像（镜像只负责传文件，下面照样按 GitHub 公布的 SHA-256 校验）
    if [ -z "${VOC_TARBALL_URL:-}" ]; then
        for m in $MIRRORS; do
            info "直连 GitHub 下载失败，改从镜像下载：$m"
            if fetch "$m$URL" "$TMP/$ASSET"; then
                ok=1
                break
            fi
        done
    fi
    [ "$ok" -eq 1 ] || die "下载失败：$URL
  服务器访问不了 GitHub 时，可以在自己电脑上下载 $ASSET 传到服务器，解压后运行 sudo bash install.sh"
fi

if [ -n "$SHA" ] && command -v sha256sum >/dev/null 2>&1; then
    GOT="$(sha256sum "$TMP/$ASSET" | cut -d' ' -f1)"
    [ "$GOT" = "$SHA" ] || die "安装包校验失败（SHA-256 不一致），已停止。请稍后重试。"
    info "SHA-256 校验通过"
else
    info "跳过 SHA-256 校验（没取到 GitHub 公布的校验值；服务器连不上 GitHub 接口时会这样）"
fi

tar -xzf "$TMP/$ASSET" -C "$TMP"
[ -f "$TMP/voc-server-linux/install.sh" ] || die "安装包内容不对"

ARGS=("$@")
if [ -n "$OLD" ]; then
    # 已安装：没给参数就是纯升级（不改配置）
    [ ${#ARGS[@]} -eq 0 ] && ARGS=(--upgrade)
    info "已安装版本 $OLD → 升级到 ${TAG:-新版本}"
else
    info "全新安装"
fi

bash "$TMP/voc-server-linux/install.sh" ${ARGS[@]+"${ARGS[@]}"}
