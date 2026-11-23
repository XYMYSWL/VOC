//! 让家用路由器把 VOC 的 UDP 端口映射到公网，别人就能直接打进来（直连成功率更高）。
//! 依次尝试三种协议，哪个成功用哪个：
//! - UPnP IGD（大多数家用路由器）
//! - PCP（RFC 6887，较新的路由器、部分运营商光猫）
//! - NAT-PMP（RFC 6886，苹果路由器等）
//! 路由器自己的外网地址如果是内网 / 运营商级 NAT（100.64.0.0/10）地址，映射没用，记为「运营商级 NAT」。

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::Duration;

/// 映射结果（网络自检里显示）
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PortMapStatus {
    /// 还在试
    Trying,
    /// 映射成功：协议名、映射出的公网地址
    Mapped { method: &'static str, external: SocketAddr },
    /// 路由器的外网地址也是内网地址：运营商级 NAT，端口映射没用
    CarrierNat { method: &'static str, router_external: Ipv4Addr },
    /// 路由器不支持（或关掉了）这三种协议
    Unavailable,
}

pub const LEASE_SECS: u32 = 3600;
const PCP_PORT: u16 = 5351;

/// 路由器外网地址是不是「其实还在内网里」
pub fn is_carrier_or_private(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_private() || ip.is_loopback() || ip.is_unspecified() || (o[0] == 100 && (64..128).contains(&o[1]))
}

// ------------------------------------------------------------ NAT-PMP（RFC 6886）

/// 查外网地址的请求
pub fn natpmp_external_req() -> [u8; 2] {
    [0, 0]
}

/// 解析外网地址回复
pub fn natpmp_external_resp(b: &[u8]) -> Option<Ipv4Addr> {
    if b.len() < 12 || b[0] != 0 || b[1] != 128 || u16::from_be_bytes([b[2], b[3]]) != 0 {
        return None;
    }
    Some(Ipv4Addr::new(b[8], b[9], b[10], b[11]))
}

/// UDP 映射请求（lifetime = 0 表示删除）
pub fn natpmp_map_req(internal: u16, suggested: u16, lifetime: u32) -> [u8; 12] {
    let mut r = [0u8; 12];
    r[1] = 1; // UDP
    r[4..6].copy_from_slice(&internal.to_be_bytes());
    r[6..8].copy_from_slice(&suggested.to_be_bytes());
    r[8..12].copy_from_slice(&lifetime.to_be_bytes());
    r
}

/// 解析映射回复：(内部端口, 外部端口, 有效期)
pub fn natpmp_map_resp(b: &[u8]) -> Option<(u16, u16, u32)> {
    if b.len() < 16 || b[0] != 0 || b[1] != 129 || u16::from_be_bytes([b[2], b[3]]) != 0 {
        return None;
    }
    Some((u16::from_be_bytes([b[8], b[9]]), u16::from_be_bytes([b[10], b[11]]), u32::from_be_bytes([b[12], b[13], b[14], b[15]])))
}

// ------------------------------------------------------------ PCP（RFC 6887）

fn v4_mapped(ip: Ipv4Addr) -> [u8; 16] {
    let mut a = [0u8; 16];
    a[10] = 0xff;
    a[11] = 0xff;
    a[12..].copy_from_slice(&ip.octets());
    a
}

/// PCP MAP 请求（60 字节）
pub fn pcp_map_req(client: Ipv4Addr, internal: u16, suggested: u16, lifetime: u32, nonce: [u8; 12]) -> [u8; 60] {
    let mut r = [0u8; 60];
    r[0] = 2; // 版本
    r[1] = 1; // MAP
    r[4..8].copy_from_slice(&lifetime.to_be_bytes());
    r[8..24].copy_from_slice(&v4_mapped(client));
    r[24..36].copy_from_slice(&nonce);
    r[36] = 17; // UDP
    r[40..42].copy_from_slice(&internal.to_be_bytes());
    r[42..44].copy_from_slice(&suggested.to_be_bytes());
    r[44..60].copy_from_slice(&v4_mapped(Ipv4Addr::UNSPECIFIED));
    r
}

/// 解析 PCP MAP 回复：(外部地址, 外部端口, 有效期)。nonce 必须对上
pub fn pcp_map_resp(b: &[u8], nonce: &[u8; 12]) -> Option<(Ipv4Addr, u16, u32)> {
    if b.len() < 60 || b[0] != 2 || b[1] != 0x81 || b[3] != 0 || &b[24..36] != nonce {
        return None;
    }
    let lifetime = u32::from_be_bytes([b[4], b[5], b[6], b[7]]);
    let port = u16::from_be_bytes([b[42], b[43]]);
    let ip = Ipv4Addr::new(b[56], b[57], b[58], b[59]);
    Some((ip, port, lifetime))
}

/// PCP PEER 请求（80 字节，第 39-3 项）：为「我这个端口 ↔ 某个对端」单独要一个映射。
/// 有的路由器不给 MAP（随便谁都能进来的映射），但给 PEER（只对这一个对端）
pub fn pcp_peer_req(client: Ipv4Addr, internal: u16, remote: SocketAddrV4, lifetime: u32, nonce: [u8; 12]) -> [u8; 80] {
    let mut r = [0u8; 80];
    r[0] = 2;
    r[1] = 2; // PEER
    r[4..8].copy_from_slice(&lifetime.to_be_bytes());
    r[8..24].copy_from_slice(&v4_mapped(client));
    r[24..36].copy_from_slice(&nonce);
    r[36] = 17;
    r[40..42].copy_from_slice(&internal.to_be_bytes());
    r[42..44].copy_from_slice(&internal.to_be_bytes());
    r[44..60].copy_from_slice(&v4_mapped(Ipv4Addr::UNSPECIFIED));
    r[60..62].copy_from_slice(&remote.port().to_be_bytes());
    r[64..80].copy_from_slice(&v4_mapped(*remote.ip()));
    r
}

/// 解析 PCP PEER 回复：(外部地址, 外部端口)
pub fn pcp_peer_resp(b: &[u8], nonce: &[u8; 12]) -> Option<(Ipv4Addr, u16)> {
    if b.len() < 80 || b[0] != 2 || b[1] != 0x82 || b[3] != 0 || &b[24..36] != nonce {
        return None;
    }
    Some((Ipv4Addr::new(b[56], b[57], b[58], b[59]), u16::from_be_bytes([b[42], b[43]])))
}

/// 网关会不会说 PCP：发一个 MAP，收到任何 PCP 回复（哪怕是拒绝）就算会。返回结果码（0 = 成功）
pub fn pcp_answers(gw: Ipv4Addr, client: Ipv4Addr, internal: u16) -> Option<u8> {
    let nonce: [u8; 12] = voc_proto::auth::random_bytes::<12>();
    let req = pcp_map_req(client, internal, internal, 60, nonce);
    let r = ask(gw, &req, |b| b.len() >= 24 && b[0] == 2 && b[1] == 0x81)?;
    Some(r[3])
}

/// 用 PCP PEER 为某个对端要映射；成功返回 (外部地址, 外部端口)
pub fn pcp_peer(gw: Ipv4Addr, client: Ipv4Addr, internal: u16, remote: SocketAddrV4, lifetime: u32) -> Option<(Ipv4Addr, u16)> {
    let nonce: [u8; 12] = voc_proto::auth::random_bytes::<12>();
    let req = pcp_peer_req(client, internal, remote, lifetime, nonce);
    let r = ask(gw, &req, |b| pcp_peer_resp(b, &nonce).is_some())?;
    pcp_peer_resp(&r, &nonce)
}

// ------------------------------------------------------------ 收发

/// 发一个请求等回复（RFC 建议的退避：250 ms 起，翻倍，试 3 次）
fn ask(gw: Ipv4Addr, req: &[u8], check: impl Fn(&[u8]) -> bool) -> Option<Vec<u8>> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    let mut wait = Duration::from_millis(250);
    let mut buf = [0u8; 1100];
    for _ in 0..3 {
        s.send_to(req, SocketAddrV4::new(gw, PCP_PORT)).ok()?;
        s.set_read_timeout(Some(wait)).ok()?;
        let deadline = std::time::Instant::now() + wait;
        while std::time::Instant::now() < deadline {
            let Ok((n, from)) = s.recv_from(&mut buf) else { break };
            if from.ip() == std::net::IpAddr::V4(gw) && check(&buf[..n]) {
                return Some(buf[..n].to_vec());
            }
        }
        wait *= 2;
    }
    None
}

/// 用 PCP 映射；成功返回 (外部地址, 外部端口)
pub fn pcp_map(gw: Ipv4Addr, client: Ipv4Addr, internal: u16, lifetime: u32) -> Option<(Ipv4Addr, u16)> {
    let nonce: [u8; 12] = voc_proto::auth::random_bytes::<12>();
    let req = pcp_map_req(client, internal, internal, lifetime, nonce);
    let r = ask(gw, &req, |b| pcp_map_resp(b, &nonce).is_some())?;
    let (ip, port, _) = pcp_map_resp(&r, &nonce)?;
    Some((ip, port))
}

/// 用 NAT-PMP 映射；成功返回 (外部地址, 外部端口)
pub fn natpmp_map(gw: Ipv4Addr, internal: u16, lifetime: u32) -> Option<(Ipv4Addr, u16)> {
    let ext = ask(gw, &natpmp_external_req(), |b| natpmp_external_resp(b).is_some()).and_then(|b| natpmp_external_resp(&b))?;
    let r = ask(gw, &natpmp_map_req(internal, internal, lifetime), |b| natpmp_map_resp(b).is_some())?;
    let (_, port, _) = natpmp_map_resp(&r)?;
    Some((ext, port))
}

/// 本机某个 IPv4 地址所在网卡的默认网关
pub fn gateway_for(lan: Ipv4Addr) -> Option<Ipv4Addr> {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::NetworkManagement::IpHelper::{GetAdaptersAddresses, GAA_FLAG_INCLUDE_GATEWAYS, IP_ADAPTER_ADDRESSES_LH};
        use windows::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};
        let mut size = 32 * 1024u32;
        let mut buf = vec![0u8; size as usize];
        let mut r = GetAdaptersAddresses(AF_INET.0 as u32, GAA_FLAG_INCLUDE_GATEWAYS, None, Some(buf.as_mut_ptr() as *mut _), &mut size);
        if r == 111 {
            // 缓冲区不够：按要求的大小重来
            buf = vec![0u8; size as usize];
            r = GetAdaptersAddresses(AF_INET.0 as u32, GAA_FLAG_INCLUDE_GATEWAYS, None, Some(buf.as_mut_ptr() as *mut _), &mut size);
        }
        if r != 0 {
            return None;
        }
        let v4 = |sa: *const windows::Win32::Networking::WinSock::SOCKADDR| -> Option<Ipv4Addr> {
            if sa.is_null() || (*sa).sa_family != AF_INET {
                return None;
            }
            let sin = &*(sa as *const SOCKADDR_IN);
            Some(Ipv4Addr::from(u32::from_be(sin.sin_addr.S_un.S_addr)))
        };
        let mut a = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
        while !a.is_null() {
            let mut u = (*a).FirstUnicastAddress;
            let mut has = false;
            while !u.is_null() {
                if v4((*u).Address.lpSockaddr) == Some(lan) {
                    has = true;
                }
                u = (*u).Next;
            }
            if has {
                let mut g = (*a).FirstGatewayAddress;
                while !g.is_null() {
                    if let Some(ip) = v4((*g).Address.lpSockaddr) {
                        return Some(ip);
                    }
                    g = (*g).Next;
                }
            }
            a = (*a).Next;
        }
        None
    }
    #[cfg(not(windows))]
    {
        let _ = lan;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natpmp_packets() {
        assert_eq!(natpmp_map_req(7878, 7878, 3600), [0, 1, 0, 0, 0x1e, 0xc6, 0x1e, 0xc6, 0, 0, 0x0e, 0x10]);
        // 回复：版本 0、操作 128+1、结果 0、时间、内部 7878、外部 40000、有效期 3600
        let resp = [0, 129, 0, 0, 0, 0, 0, 5, 0x1e, 0xc6, 0x9c, 0x40, 0, 0, 0x0e, 0x10];
        assert_eq!(natpmp_map_resp(&resp), Some((7878, 40000, 3600)));
        let mut bad = resp;
        bad[3] = 3; // 结果码不是 0
        assert_eq!(natpmp_map_resp(&bad), None);
        let ext = [0, 128, 0, 0, 0, 0, 0, 5, 203, 0, 113, 7];
        assert_eq!(natpmp_external_resp(&ext), Some(Ipv4Addr::new(203, 0, 113, 7)));
    }

    #[test]
    fn pcp_packets() {
        let nonce = [9u8; 12];
        let req = pcp_map_req(Ipv4Addr::new(192, 168, 1, 5), 7878, 7878, 3600, nonce);
        assert_eq!((req[0], req[1], req[36]), (2, 1, 17));
        assert_eq!(&req[8..24], &v4_mapped(Ipv4Addr::new(192, 168, 1, 5)));
        // 路由器回复：把请求改成回复格式
        let mut resp = req;
        resp[1] = 0x81;
        resp[3] = 0;
        resp[42..44].copy_from_slice(&41000u16.to_be_bytes());
        resp[44..60].copy_from_slice(&v4_mapped(Ipv4Addr::new(203, 0, 113, 9)));
        assert_eq!(pcp_map_resp(&resp, &nonce), Some((Ipv4Addr::new(203, 0, 113, 9), 41000, 3600)));
        assert_eq!(pcp_map_resp(&resp, &[1u8; 12]), None, "nonce mismatch");
        resp[3] = 8; // 错误码
        assert_eq!(pcp_map_resp(&resp, &nonce), None);
    }

    #[test]
    fn pcp_peer_packets() {
        let nonce = [7u8; 12];
        let remote = SocketAddrV4::new(Ipv4Addr::new(198, 51, 100, 7), 40123);
        let req = pcp_peer_req(Ipv4Addr::new(192, 168, 1, 5), 7878, remote, 120, nonce);
        assert_eq!((req.len(), req[0], req[1], req[36]), (80, 2, 2, 17));
        assert_eq!(&req[60..62], &40123u16.to_be_bytes());
        assert_eq!(&req[64..80], &v4_mapped(*remote.ip()));
        let mut resp = req;
        resp[1] = 0x82;
        resp[42..44].copy_from_slice(&50000u16.to_be_bytes());
        resp[44..60].copy_from_slice(&v4_mapped(Ipv4Addr::new(203, 0, 113, 9)));
        assert_eq!(pcp_peer_resp(&resp, &nonce), Some((Ipv4Addr::new(203, 0, 113, 9), 50000)));
        resp[3] = 2; // NOT_AUTHORIZED
        assert_eq!(pcp_peer_resp(&resp, &nonce), None);
    }

    #[test]
    fn carrier_nat_detection() {
        assert!(is_carrier_or_private(Ipv4Addr::new(100, 72, 1, 1)));
        assert!(is_carrier_or_private(Ipv4Addr::new(10, 0, 0, 1)));
        assert!(!is_carrier_or_private(Ipv4Addr::new(203, 0, 113, 9)));
    }

    #[test]
    fn gateway_of_this_machine() {
        // 本机有默认路由时应能找到网关（CI 机器可能没有，就不断言）
        if let Ok(s) = UdpSocket::bind("0.0.0.0:0") {
            if s.connect("8.8.8.8:53").is_ok() {
                if let Ok(SocketAddr::V4(a)) = s.local_addr() {
                    println!("本机 {} 的网关：{:?}", a.ip(), gateway_for(*a.ip()));
                }
            }
        }
    }
}
