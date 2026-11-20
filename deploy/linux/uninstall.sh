#!/usr/bin/env bash
# VOC 服务器卸载脚本
#   sudo bash uninstall.sh           卸载程序和服务，保留授权码与配置（重装后继续可用）
#   sudo bash uninstall.sh --purge   连同授权码、配置和 voc 系统用户一起删除（不可恢复）
set -euo pipefail

[ "$(id -u)" -eq 0 ] || { echo "请用 root 运行：sudo bash $0"; exit 1; }
PURGE=0
[ "${1:-}" = "--purge" ] && PURGE=1

systemctl disable --now voc-server >/dev/null 2>&1 || true
rm -f /etc/systemd/system/voc-server.service /usr/local/bin/voc-server /usr/local/bin/voc
systemctl daemon-reload || true

if [ "$PURGE" -eq 1 ]; then
    rm -rf /var/lib/voc /etc/voc
    userdel voc >/dev/null 2>&1 || true
    echo "已彻底删除 VOC 服务器（包括授权码和配置）。"
else
    echo "已卸载 VOC 服务器。授权码保留在 /var/lib/voc，配置保留在 /etc/voc。"
    echo "彻底删除请运行：sudo bash $0 --purge"
fi
echo "注意：防火墙里放行的端口没有自动关闭，如需关闭请手动处理（ufw delete / firewall-cmd --remove-port）。"
