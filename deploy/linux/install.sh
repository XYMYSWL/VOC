#!/usr/bin/env bash
# VOC 服务器一键安装 / 升级脚本（Linux + systemd）
#
# 用法（在解压后的目录里）：
#   sudo bash install.sh                                  # 默认端口 7878
#   sudo bash install.sh --port 443 --name "我的 VOC 服"  # 自定义端口、名称
#   sudo bash install.sh --channels "大厅,五排,双排,挂机"
#   sudo bash install.sh --upgrade                        # 只更新程序，保留配置和授权码
#   sudo bash install.sh --max-video-mbps 1.5             # 服务器转发屏幕共享画面最多 1.5 Mbps（带宽小的服务器用）
#
# 再次运行即为升级：授权码（/var/lib/voc）和配置（/etc/voc）都会保留。
# 不想手动下载解压？用一键脚本：curl -fsSL https://github.com/XYMYSWL/VOC/releases/latest/download/get.sh | sudo bash

set -euo pipefail

PORT=""
NAME=""
CHANNELS=""
BIND_IP=""
UPGRADE=0
FIREWALL=1
MAX_VIDEO=""

BIN=/usr/local/bin/voc-server
ADMIN=/usr/local/bin/voc
LIB=/usr/local/lib/voc
CONF_DIR=/etc/voc
CONF=$CONF_DIR/voc.conf
DATA=/var/lib/voc
CODES=$DATA/voc-codes.json
UNIT=/etc/systemd/system/voc-server.service
SVC_USER=voc

red()   { printf '\033[31m%s\033[0m\n' "$*"; }
green() { printf '\033[32m%s\033[0m\n' "$*"; }
info()  { printf '\033[36m==>\033[0m %s\n' "$*"; }
die()   { red "错误：$*"; exit 1; }

usage() {
    sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'
    cat <<'EOF'
选项：
  --port <端口>         监听端口（TCP+UDP 同端口），默认 7878；想更好地混在普通流量里可用 443
  --name <名称>         服务器名称
  --channels <a,b,c>    初始语音频道
  --bind-ip <IP>        只监听某个地址，默认 0.0.0.0（全部 IPv4）；用 :: 可同时监听 IPv6
  --upgrade             只替换程序并重启，不改配置
  --max-video-mbps <N>  服务器转发屏幕共享画面的总带宽上限（Mbps，可以带小数；off = 不限）。
                        超出时让部分观看者自动改看「流畅」。装好后也能改：sudo voc video-limit 1.5
  --no-firewall         不自动放行防火墙端口
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --port) PORT="${2:-}"; shift 2 ;;
        --name) NAME="${2:-}"; shift 2 ;;
        --channels) CHANNELS="${2:-}"; shift 2 ;;
        --bind-ip) BIND_IP="${2:-}"; shift 2 ;;
        --upgrade) UPGRADE=1; shift ;;
        --max-video-mbps) MAX_VIDEO="${2:-}"; shift 2 ;;
        --no-firewall) FIREWALL=0; shift ;;
        -h|--help) usage; exit 0 ;;
        *) die "未知参数：$1（用 --help 查看）" ;;
    esac
done

[ "$(id -u)" -eq 0 ] || die "请用 root 运行：sudo bash $0"
command -v systemctl >/dev/null 2>&1 || die "需要 systemd（Ubuntu 16.04+ / Debian 9+ / CentOS 7+ 等都有）"

if [ -n "$PORT" ]; then
    case "$PORT" in ''|*[!0-9]*) die "端口必须是数字" ;; esac
    [ "$PORT" -ge 1 ] && [ "$PORT" -le 65535 ] || die "端口范围 1-65535"
fi
# 画面限速 → 配置里的 VOC_VIDEO_ARGS
VIDEO_ARGS=""
if [ -n "$MAX_VIDEO" ]; then
    case "$MAX_VIDEO" in
        off|0) VIDEO_ARGS="" ;;
        *) printf '%s' "$MAX_VIDEO" | grep -Eq '^[0-9]+(\.[0-9]+)?$' || die "--max-video-mbps 要填数字（Mbps），例如 1.5，或 off"
           VIDEO_ARGS="--max-video-mbps $MAX_VIDEO" ;;
    esac
fi

# 改配置文件里的一项（没有就加上）
set_conf() {
    if grep -q "^$1=" "$CONF"; then
        sed -i "s|^$1=.*|$1=\"$2\"|" "$CONF"
    else
        printf '%s="%s"\n' "$1" "$2" >> "$CONF"
    fi
}

# ---------------------------------------------------------------- 选择程序
HERE="$(cd "$(dirname "$0")" && pwd)"
case "$(uname -m)" in
    x86_64|amd64) ARCH=x86_64 ;;
    aarch64|arm64) ARCH=aarch64 ;;
    *) die "不支持的 CPU 架构：$(uname -m)（目前提供 x86_64 和 arm64）" ;;
esac
SRC="$HERE/bin/$ARCH/voc-server"
[ -f "$SRC" ] || SRC="$HERE/voc-server"
[ -f "$SRC" ] || die "找不到程序文件 bin/$ARCH/voc-server，请确认在解压后的目录里运行"

info "安装程序（$ARCH）→ $BIN"
id "$SVC_USER" >/dev/null 2>&1 || useradd --system --home-dir "$DATA" --shell /usr/sbin/nologin "$SVC_USER" 2>/dev/null \
    || useradd -r -d "$DATA" -s /sbin/nologin "$SVC_USER"
install -d -m 0750 -o "$SVC_USER" -g "$SVC_USER" "$DATA"
install -d -m 0755 "$CONF_DIR"
# 先装到临时文件再替换，运行中的服务不受影响
install -m 0755 "$SRC" "$BIN.new"
mv -f "$BIN.new" "$BIN"
"$BIN" --help >/dev/null 2>&1 || die "程序无法运行（架构不匹配或文件损坏）"

# ---------------------------------------------------------------- 配置
if [ -f "$CONF" ]; then
    # shellcheck disable=SC1090
    . "$CONF"
fi
if [ "$UPGRADE" -eq 0 ] || [ ! -f "$CONF" ]; then
    : "${VOC_PORT:=7878}" "${VOC_NAME:=VOC 服务器}" "${VOC_CHANNELS:=大厅,开黑一号,开黑二号,挂机}" "${VOC_BIND_IP:=0.0.0.0}"
    [ -n "$PORT" ] && VOC_PORT="$PORT"
    [ -n "$NAME" ] && VOC_NAME="$NAME"
    [ -n "$CHANNELS" ] && VOC_CHANNELS="$CHANNELS"
    [ -n "$BIND_IP" ] && VOC_BIND_IP="$BIND_IP"
    case "$VOC_BIND_IP" in *:*) VOC_BIND="[$VOC_BIND_IP]:$VOC_PORT" ;; *) VOC_BIND="$VOC_BIND_IP:$VOC_PORT" ;; esac
    info "写入配置 $CONF"
    cat > "$CONF" <<EOF
# VOC 服务器配置。修改后执行：sudo voc restart
VOC_PORT="$VOC_PORT"
VOC_BIND_IP="$VOC_BIND_IP"
VOC_BIND="$VOC_BIND"
VOC_NAME="$VOC_NAME"
VOC_CHANNELS="$VOC_CHANNELS"
# 服务器转发屏幕共享画面的总带宽上限，用命令改：sudo voc video-limit 1.5（Mbps）/ sudo voc video-limit off
VOC_VIDEO_ARGS="${VOC_VIDEO_ARGS:-}"
# 其他附加参数
VOC_EXTRA_ARGS="${VOC_EXTRA_ARGS:-}"
EOF
    chmod 0644 "$CONF"
fi
if [ -n "$MAX_VIDEO" ]; then
    # 以前写在附加参数里的画面限速先去掉，免得两处打架
    sed -i 's/--max-video-mbps [0-9.]*//' "$CONF"
    set_conf VOC_VIDEO_ARGS "$VIDEO_ARGS"
    if [ -n "$VIDEO_ARGS" ]; then info "画面限速：$MAX_VIDEO Mbps"; else info "画面限速：不限"; fi
fi
# shellcheck disable=SC1090
. "$CONF"

# ---------------------------------------------------------------- systemd
info "配置 systemd 服务 voc-server"
cat > "$UNIT" <<EOF
[Unit]
Description=VOC voice chat server
Documentation=file://$CONF
After=network-online.target time-sync.target
Wants=network-online.target

[Service]
Type=simple
User=$SVC_USER
Group=$SVC_USER
EnvironmentFile=$CONF
WorkingDirectory=$DATA
ExecStart=$BIN --bind \${VOC_BIND} --name \${VOC_NAME} --channels \${VOC_CHANNELS} --codes $CODES \$VOC_VIDEO_ARGS \$VOC_EXTRA_ARGS
Restart=always
RestartSec=2
LimitNOFILE=65536
# 允许以普通用户身份监听 1024 以下端口（例如 443）
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
# 安全加固
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=$DATA
PrivateTmp=true
PrivateDevices=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
RestrictSUIDSGID=true
RestrictNamespaces=true
LockPersonality=true
MemoryDenyWriteExecute=true
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX
SystemCallArchitectures=native
UMask=0077

[Install]
WantedBy=multi-user.target
EOF

# ---------------------------------------------------------------- 管理命令
info "安装管理命令 $ADMIN"
cat > "$ADMIN" <<EOF
#!/usr/bin/env bash
# VOC 服务器管理（需要 root）
#   voc gen-code [-n 数量] [--note 备注] [--days 天数]   生成授权码
#   voc list-codes                                       查看授权码
#   voc revoke-code <ID> / voc restore-code <ID>          吊销 / 恢复
#   voc status                                           运行状态（在线的人、谁在共享、转发带宽、NAT 探测端口）
#   voc logs | restart | stop | start                    服务管理
#   voc config                                           编辑配置
#   voc video-limit [Mbps|off]                           服务器转发屏幕共享画面的上限（不带参数 = 查看）
#   voc upgrade [vX.Y.Z]                                 升级到最新版（或指定版本），保留授权码和配置
#   voc version                                          显示版本
set -e
[ "\$(id -u)" -eq 0 ] || { echo "请用 sudo 运行：sudo voc \$*"; exit 1; }
set_conf() {
    if grep -q "^\$1=" $CONF; then
        sed -i "s|^\$1=.*|\$1=\"\$2\"|" $CONF
    else
        printf '%s="%s"\n' "\$1" "\$2" >> $CONF
    fi
}
case "\${1:-}" in
    video-limit)
        V="\${2:-}"
        CUR="\$(. $CONF; printf '%s %s' "\${VOC_VIDEO_ARGS:-}" "\${VOC_EXTRA_ARGS:-}" | grep -o -- '--max-video-mbps [0-9.]*' | head -1 | awk '{print \$2}')"
        if [ -z "\$V" ]; then
            if [ -n "\$CUR" ]; then echo "服务器转发屏幕共享画面：最多 \$CUR Mbps"; else echo "服务器转发屏幕共享画面：不限"; fi
            echo "修改：sudo voc video-limit 1.5（单位 Mbps，可带小数）/ sudo voc video-limit off（不限）"
            exit 0
        fi
        case "\$V" in
            off|0) NEW="" ;;
            *) printf '%s' "\$V" | grep -Eq '^[0-9]+(\.[0-9]+)?\$' || { echo "要填数字（Mbps），例如 1.5，或 off"; exit 1; }
               NEW="--max-video-mbps \$V" ;;
        esac
        sed -i 's/--max-video-mbps [0-9.]*//' $CONF
        set_conf VOC_VIDEO_ARGS "\$NEW"
        echo "已保存。重启服务生效：在线的人会断开一两秒，然后自动重连……"
        systemctl restart voc-server
        if [ -n "\$NEW" ]; then echo "服务器转发屏幕共享画面：最多 \$V Mbps（超出时部分观看者自动改看「流畅」）"; else echo "服务器转发屏幕共享画面：不限"; fi ;;
    status)
        systemctl status voc-server --no-pager -n 0 || true
        echo
        exec $BIN status --codes $CODES ;;
    logs) exec journalctl -u voc-server -f -n 100 ;;
    restart|stop|start) exec systemctl "\$1" voc-server ;;
    config) "\${EDITOR:-nano}" $CONF && systemctl restart voc-server && echo "已重启生效" ;;
    version) exec $BIN --version ;;
    upgrade)
        [ -n "\${2:-}" ] && export VOC_VERSION="\$2"
        exec bash $LIB/get.sh --upgrade ;;
    gen-code|list-codes|revoke-code|restore-code)
        cd $DATA
        if command -v runuser >/dev/null 2>&1; then
            exec runuser -u $SVC_USER -- $BIN "\$@" --codes $CODES
        else
            exec su -s /bin/sh $SVC_USER -c '$BIN "\$@" --codes $CODES' -- sh "\$@"
        fi ;;
    *) sed -n '2,11p' "\$0" | sed 's/^# \{0,1\}//'; exit 1 ;;
esac
EOF
chmod 0755 "$ADMIN"
if [ -f "$HERE/get.sh" ]; then
    install -d -m 0755 "$LIB"
    install -m 0755 "$HERE/get.sh" "$LIB/get.sh"
fi

# ---------------------------------------------------------------- 时间同步（握手要求时钟偏差 < 10 分钟）
if command -v timedatectl >/dev/null 2>&1; then
    timedatectl set-ntp true >/dev/null 2>&1 || true
fi

# ---------------------------------------------------------------- 防火墙
# NAT 探测端口（服务器默认用主端口 + 1，帮客户端判断 NAT 类型）
PROBE_PORT=$((VOC_PORT + 1))
if [ "$FIREWALL" -eq 1 ]; then
    if command -v ufw >/dev/null 2>&1 && ufw status 2>/dev/null | grep -q "Status: active"; then
        info "ufw 放行 $VOC_PORT/tcp、$VOC_PORT/udp 和 NAT 探测端口 $PROBE_PORT/udp"
        ufw allow "$VOC_PORT/tcp" >/dev/null
        ufw allow "$VOC_PORT/udp" >/dev/null
        ufw allow "$PROBE_PORT/udp" >/dev/null
    elif command -v firewall-cmd >/dev/null 2>&1 && firewall-cmd --state >/dev/null 2>&1; then
        info "firewalld 放行 $VOC_PORT/tcp、$VOC_PORT/udp 和 NAT 探测端口 $PROBE_PORT/udp"
        firewall-cmd --permanent --add-port="$VOC_PORT/tcp" >/dev/null
        firewall-cmd --permanent --add-port="$VOC_PORT/udp" >/dev/null
        firewall-cmd --permanent --add-port="$PROBE_PORT/udp" >/dev/null
        firewall-cmd --reload >/dev/null
    else
        info "未检测到启用中的 ufw / firewalld，跳过本机防火墙设置"
    fi
fi

# ---------------------------------------------------------------- 启动
info "启动服务"
systemctl daemon-reload
systemctl enable voc-server >/dev/null 2>&1
systemctl restart voc-server
sleep 1
if ! systemctl is-active --quiet voc-server; then
    red "服务没有启动成功，最近日志："
    journalctl -u voc-server -n 30 --no-pager || true
    exit 1
fi

# 第一次安装：自动生成一个管理员授权码
FIRST_CODE=""
if [ ! -s "$CODES" ] || ! grep -q '"id"' "$CODES" 2>/dev/null; then
    FIRST_CODE="$("$ADMIN" gen-code --note 管理员 | grep '^KH-' || true)"
fi

ADDRS="$(hostname -I 2>/dev/null | tr ' ' '\n' | grep -v '^$' | head -3 | tr '\n' ' ' || true)"
echo
green "✔ VOC 服务器已安装并在运行（开机自启）"
echo "  名称：$VOC_NAME"
echo "  端口：$VOC_PORT（TCP + UDP）"
echo "  本机地址：${ADDRS:-（未知）}  ← 客户端要填的是服务器的「公网 IP 或域名」"
[ "$VOC_PORT" != "7878" ] && echo "  客户端填写：公网IP:$VOC_PORT"
if [ -n "$FIRST_CODE" ]; then
    echo
    green "  第一个授权码（只显示这一次，请保存好）："
    echo "  $FIRST_CODE"
fi
echo
echo "常用命令："
echo "  sudo voc gen-code --note 小明        给小明生成授权码"
echo "  sudo voc gen-code -n 5 --days 30     生成 5 个 30 天有效的授权码"
echo "  sudo voc list-codes / revoke-code <ID>"
echo "  sudo voc status / logs / restart"
echo "  sudo voc video-limit 1.5             服务器带宽小：转发屏幕共享画面最多 1.5 Mbps（off = 不限）"
echo "  sudo voc upgrade                     升级到最新版（保留授权码和配置）"
echo
red "⚠ 如果是云服务器（阿里云、腾讯云、AWS 等），还要在控制台的「安全组 / 防火墙」里放行 TCP 和 UDP 的 $VOC_PORT 端口，以及 UDP $PROBE_PORT（NAT 探测，可选）。"
