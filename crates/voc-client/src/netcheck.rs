//! 网络自检：把链路上已有的测量（UDP 通不通、到服务器的延迟和丢包、服务器看到的公网地址、
//! 路由器端口映射、打洞结果）整理成一张表和几条大白话建议。不额外发包。

use crate::portmap::PortMapStatus;
use std::net::{IpAddr, SocketAddr};

/// 链路快照（[`crate::voice::Link::netcheck_snapshot`]）
#[derive(Clone, Debug)]
pub struct Snapshot {
    /// 连上多久了（秒）：刚连上时很多项还没结果
    pub age_secs: u64,
    pub udp_ok: bool,
    /// 收到过服务器的 UDP 回复
    pub ever_pong: bool,
    pub server_rtt: Option<u32>,
    pub server_loss: Option<u8>,
    /// 服务器看到的我的公网地址（旧服务器不告诉）
    pub public: Option<SocketAddr>,
    pub lan: Vec<SocketAddr>,
    pub local_port: u16,
    pub portmap: PortMapStatus,
    pub p2p: bool,
    pub punched: usize,
    pub punching: usize,
    pub failed: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Good,
    Warn,
    Bad,
    /// 还在测
    Pending,
    Info,
}

#[derive(Clone, Debug)]
pub struct Item {
    pub level: Level,
    pub label: &'static str,
    pub value: String,
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub items: Vec<Item>,
    pub advice: Vec<String>,
}

/// 连上这么多秒后还没结果才下结论
const SETTLE_SECS: u64 = 6;

pub fn analyze(s: &Snapshot) -> Report {
    let mut r = Report::default();
    let settled = s.age_secs >= SETTLE_SECS;
    let mut item = |level, label, value: String| r.items.push(Item { level, label, value });

    // UDP
    let udp_blocked = !s.ever_pong && settled;
    let (lv, v) = if s.udp_ok {
        (Level::Good, "通".to_string())
    } else if udp_blocked {
        (Level::Bad, "不通，已改走 TCP 隧道".into())
    } else if s.ever_pong {
        (Level::Warn, "时断时续，已改走 TCP 隧道".into())
    } else {
        (Level::Pending, "检测中…".into())
    };
    item(lv, "UDP", v);

    // 延迟、丢包
    match s.server_rtt {
        Some(ms) => item(
            if ms < 80 {
                Level::Good
            } else if ms < 200 {
                Level::Warn
            } else {
                Level::Bad
            },
            "到服务器延迟",
            format!("{ms} ms"),
        ),
        None => item(if udp_blocked { Level::Info } else { Level::Pending }, "到服务器延迟", if udp_blocked { "—".into() } else { "检测中…".into() }),
    }
    match s.server_loss {
        Some(p) => item(
            if p <= 1 {
                Level::Good
            } else if p <= 5 {
                Level::Warn
            } else {
                Level::Bad
            },
            "到服务器丢包",
            format!("{p}%（最近 30 秒）"),
        ),
        None => item(if udp_blocked { Level::Info } else { Level::Pending }, "到服务器丢包", if udp_blocked { "—".into() } else { "统计中…".into() }),
    }

    // 地址
    let lan_v4: Vec<String> = s.lan.iter().filter(|a| a.is_ipv4()).map(|a| a.ip().to_string()).collect();
    let has_v6 = s.lan.iter().any(|a| matches!(a.ip(), IpAddr::V6(ip) if (ip.segments()[0] & 0xe000) == 0x2000));
    item(
        Level::Info,
        "本机地址",
        format!(
            "{}{}",
            if lan_v4.is_empty() { "无 IPv4".to_string() } else { lan_v4.join("、") },
            if has_v6 { " · 有公网 IPv6" } else { " · 没有公网 IPv6" }
        ),
    );
    match s.public {
        Some(p) => item(Level::Info, "公网地址", format!("{}（服务器看到的）", p.ip())),
        None if settled && s.ever_pong => item(Level::Info, "公网地址", "未知（服务器版本较旧）".into()),
        None => item(Level::Pending, "公网地址", if udp_blocked { "—".into() } else { "检测中…".into() }),
    }

    // NAT：公网地址就在本机上 = 没有 NAT；端口没变 = 打洞容易
    let no_nat = s.public.is_some_and(|p| s.lan.iter().any(|l| l.ip() == p.ip().to_canonical()));
    let port_changed = s.public.is_some_and(|p| p.port() != s.local_port);
    if let Some(_p) = s.public {
        if no_nat {
            item(Level::Good, "NAT", "没有（本机直接有公网地址）".into());
        } else if !port_changed {
            item(Level::Good, "NAT", "端口保持不变（打洞容易成功）".into());
        } else {
            item(Level::Warn, "NAT", "会改端口（打洞较难，可能是对称型 NAT）".into());
        }
    }

    // 端口映射
    let mut double_nat = false;
    match &s.portmap {
        PortMapStatus::Trying => item(Level::Pending, "端口映射", "正在问路由器…".into()),
        PortMapStatus::Mapped { method, external } => {
            double_nat = s.public.is_some_and(|p| p.is_ipv4() && p.ip().to_canonical() != external.ip());
            if double_nat {
                item(Level::Warn, "端口映射", format!("{method} 映射成功，但上游还有一层 NAT（光猫或运营商），映射可能无效"));
            } else {
                item(Level::Good, "端口映射", format!("{method} 已映射到 {external}"));
            }
        }
        PortMapStatus::CarrierNat { method, router_external } => item(
            Level::Bad,
            "端口映射",
            format!("没用：路由器的外网地址 {router_external} 不是公网地址（运营商级 NAT，{method} 查到的）"),
        ),
        PortMapStatus::Unavailable => item(
            if no_nat { Level::Info } else { Level::Warn },
            "端口映射",
            if no_nat { "不需要".into() } else { "路由器不支持或没开 UPnP / NAT-PMP / PCP".into() },
        ),
    }

    // 直连
    if !s.p2p {
        item(Level::Info, "P2P 直连", "已关闭".into());
    } else if s.punched + s.punching + s.failed == 0 {
        item(Level::Info, "P2P 直连", "同频道还没有能直连的人".into());
    } else {
        item(
            if s.failed == 0 { Level::Good } else { Level::Warn },
            "P2P 直连",
            format!("已打通 {} · 打洞中 {} · 打不通 {}", s.punched, s.punching, s.failed),
        );
    }

    // 建议
    let a = &mut r.advice;
    if udp_blocked {
        a.push("UDP 被封锁（常见于公司、学校网络或防火墙），语音已改走 TCP，能用但延迟高一些、卡顿时恢复慢。换个网络或请管理员放行 UDP 可以改善。".into());
    } else if s.ever_pong && !s.udp_ok {
        a.push("UDP 时断时续，暂时走 TCP。可能是 Wi-Fi 信号差或网络拥堵。".into());
    }
    if s.server_loss.is_some_and(|p| p > 5) {
        a.push("到服务器丢包较多，声音会断断续续。用网线代替 Wi-Fi、靠近路由器，或者关掉正在下载 / 看视频的设备。".into());
    } else if s.server_loss.is_some_and(|p| p > 1) {
        a.push("有少量丢包，一般听不出来；如果偶尔卡顿，优先检查 Wi-Fi。".into());
    }
    if s.server_rtt.is_some_and(|ms| ms >= 200) {
        a.push("到服务器延迟很高，可能是服务器离你太远或线路绕路。能直连的队友不受影响。".into());
    }
    if matches!(s.portmap, PortMapStatus::CarrierNat { .. }) || double_nat {
        a.push("你在运营商级 NAT 或多层 NAT 后面，和别人打洞可能失败（失败会自动走服务器，不影响使用）。可以打电话给运营商申请公网 IP，或者在光猫里开启 IPv6。".into());
    } else if matches!(s.portmap, PortMapStatus::Unavailable) && !no_nat && port_changed {
        a.push("在路由器管理页面里打开 UPnP，可以明显提高和队友直连的成功率。".into());
    }
    if s.p2p && s.failed > 0 {
        a.push(format!("和 {} 人打不通直连，他们的声音经服务器或队友中转，延迟稍高但不影响使用。", s.failed));
    }
    if !s.p2p {
        a.push("P2P 直连关着：所有声音都经服务器中转。打开后和队友的延迟一般会更低。".into());
    }
    if a.is_empty() && settled && s.udp_ok {
        a.push("网络状况良好 ✓".into());
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Snapshot {
        Snapshot {
            age_secs: 30,
            udp_ok: true,
            ever_pong: true,
            server_rtt: Some(40),
            server_loss: Some(0),
            public: Some("1.2.3.4:50000".parse().unwrap()),
            lan: vec!["192.168.0.5:50000".parse().unwrap()],
            local_port: 50000,
            portmap: PortMapStatus::Mapped { method: "UPnP", external: "1.2.3.4:50000".parse().unwrap() },
            p2p: true,
            punched: 2,
            punching: 0,
            failed: 0,
        }
    }

    fn level(r: &Report, label: &str) -> Level {
        r.items.iter().find(|i| i.label == label).unwrap().level
    }

    #[test]
    fn healthy_network() {
        let r = analyze(&base());
        assert!(r.items.iter().all(|i| matches!(i.level, Level::Good | Level::Info)), "{:?}", r.items);
        assert_eq!(r.advice, vec!["网络状况良好 ✓".to_string()]);
    }

    #[test]
    fn udp_blocked() {
        let mut s = base();
        (s.udp_ok, s.ever_pong, s.server_rtt, s.server_loss, s.public) = (false, false, None, None, None);
        let r = analyze(&s);
        assert_eq!(level(&r, "UDP"), Level::Bad);
        assert!(r.advice[0].contains("UDP 被封锁"));
        // 刚连上时还不下结论
        s.age_secs = 1;
        assert_eq!(level(&analyze(&s), "UDP"), Level::Pending);
    }

    #[test]
    fn carrier_and_double_nat() {
        let mut s = base();
        s.portmap = PortMapStatus::CarrierNat { method: "NAT-PMP", router_external: "100.70.1.2".parse().unwrap() };
        let r = analyze(&s);
        assert_eq!(level(&r, "端口映射"), Level::Bad);
        assert!(r.advice.iter().any(|a| a.contains("运营商级 NAT")));
        // 映射出的地址和服务器看到的不一样 = 多层 NAT
        s.portmap = PortMapStatus::Mapped { method: "UPnP", external: "5.6.7.8:50000".parse().unwrap() };
        assert_eq!(level(&analyze(&s), "端口映射"), Level::Warn);
    }

    #[test]
    fn port_changing_nat_without_upnp() {
        let mut s = base();
        s.public = Some("1.2.3.4:61000".parse().unwrap());
        s.portmap = PortMapStatus::Unavailable;
        (s.punched, s.failed) = (1, 1);
        let r = analyze(&s);
        assert_eq!(level(&r, "NAT"), Level::Warn);
        assert!(r.advice.iter().any(|a| a.contains("UPnP")));
        assert!(r.advice.iter().any(|a| a.contains("1 人打不通")));
    }

    #[test]
    fn no_nat_and_lossy() {
        let mut s = base();
        s.lan = vec!["1.2.3.4:50000".parse().unwrap()];
        s.portmap = PortMapStatus::Unavailable;
        s.server_loss = Some(9);
        let r = analyze(&s);
        assert_eq!(level(&r, "NAT"), Level::Good);
        assert_eq!(level(&r, "端口映射"), Level::Info);
        assert_eq!(level(&r, "到服务器丢包"), Level::Bad);
        assert!(r.advice.iter().any(|a| a.contains("网线")));
    }
}
