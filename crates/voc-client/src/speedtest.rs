//! 测上行带宽（第 71 项，设置 → 网络「测上行带宽」，只在用户点按钮时测）：
//! 1. 自己的 VOC 服务器（服务器要支持 FEATURE_SPEEDTEST）：几秒内逐步加速发 UDP 测速包，服务器按 250 ms 数收到多少，取最快那几段
//! 2. Cloudflare 测速（speed.cloudflare.com）：逐步加大的 HTTP 上传，按用时算
//! 3. Leaseweb 香港公共 iperf3 节点：标准 iperf3 上传测试（一次只让一个人测，忙的时候跳过）
//! 测完去掉比中位数低一半以上的（那条线路正好堵），其余取平均，作为「自动选择的上行带宽」。
//! 南京大学、中国科大的测速站对校外程序访问有防机器人验证 / 工作量证明，不用。

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default)]
pub struct Row {
    pub name: String,
    pub latency_ms: Option<u32>,
    pub mbps: Option<f32>,
    /// 失败原因 / 备注
    pub note: String,
    /// 算平均时被当成离谱的低值去掉了
    pub dropped: bool,
}

#[derive(Default)]
pub struct State {
    pub running: bool,
    pub rows: Vec<Row>,
    /// 去掉离谱低值后的平均（Mbps）
    pub chosen: Option<f32>,
    /// 测完、还没应用到设置里
    pub fresh: bool,
}

/// 服务器回来的测速结果（每 250 ms 的千比特每秒），由界面收到 ServerMsg::SpeedTestResult 后放进来
pub static SERVER_RESULT: Mutex<Option<Vec<u32>>> = Mutex::new(None);

/// 去掉比中位数低一半以上的，其余平均；返回 (平均, 各项是否被去掉)
pub fn robust_average(v: &[f32]) -> Option<(f32, Vec<bool>)> {
    if v.is_empty() {
        return None;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    let median = if s.len() % 2 == 1 { s[s.len() / 2] } else { (s[s.len() / 2 - 1] + s[s.len() / 2]) / 2.0 };
    let dropped: Vec<bool> = v.iter().map(|x| *x < median * 0.5).collect();
    let kept: Vec<f32> = v.iter().zip(&dropped).filter(|(_, d)| !**d).map(|(x, _)| *x).collect();
    Some((kept.iter().sum::<f32>() / kept.len().max(1) as f32, dropped))
}

/// 在后台线程里测；send_probe(字节数) 往自己的服务器发一个测速包，start / stop 通知服务器开始 / 结束
pub fn spawn(
    state: Arc<Mutex<State>>,
    ctx: eframe::egui::Context,
    server: Option<(u32, Box<dyn Fn(bool) + Send>, Box<dyn Fn(usize) + Send>)>,
) {
    {
        let mut s = state.lock().unwrap();
        if s.running {
            return;
        }
        *s = State { running: true, ..Default::default() };
    }
    std::thread::spawn(move || {
        let push = |r: Row| {
            state.lock().unwrap().rows.push(r);
            ctx.request_repaint();
        };
        match server {
            Some((rtt, control, probe)) => push(test_own_server(rtt, control, probe)),
            None => push(Row { name: "你的 VOC 服务器".into(), note: "服务器版本太旧，不支持测速（升级到 0.6.5 后可用）".into(), ..Default::default() }),
        }
        push(test_cloudflare());
        push(test_iperf3("Leaseweb 香港（iperf3）", "speedtest.hkg12.hk.leaseweb.net", &[5201, 5202, 5203]));
        let vals: Vec<(usize, f32)> = {
            let s = state.lock().unwrap();
            s.rows.iter().enumerate().filter_map(|(i, r)| r.mbps.map(|m| (i, m))).collect()
        };
        let mut s = state.lock().unwrap();
        if let Some((avg, dropped)) = robust_average(&vals.iter().map(|x| x.1).collect::<Vec<_>>()) {
            for ((i, _), d) in vals.iter().zip(dropped) {
                s.rows[*i].dropped = d;
            }
            s.chosen = Some(avg);
            s.fresh = true;
        }
        s.running = false;
        ctx.request_repaint();
    });
}

/// 自己的服务器：4 秒内从 4 Mbps 起每 0.5 秒翻倍（最多 400 Mbps）地发，服务器按 250 ms 数，取收到最快的两段平均
fn test_own_server(rtt: u32, control: Box<dyn Fn(bool) + Send>, probe: Box<dyn Fn(usize) + Send>) -> Row {
    let mut row = Row { name: "你的 VOC 服务器".into(), latency_ms: (rtt > 0).then_some(rtt), ..Default::default() };
    *SERVER_RESULT.lock().unwrap() = None;
    control(true);
    std::thread::sleep(Duration::from_millis(300));
    const PKT: usize = 1200;
    let t0 = Instant::now();
    let mut sent = 0f64;
    while t0.elapsed() < Duration::from_secs(4) {
        let t = t0.elapsed().as_secs_f64();
        let rate_bps = (4e6 * 2f64.powf((t / 0.5).floor())).min(400e6);
        // 按「该发多少」补发（Windows 上 sleep 精度只有十几毫秒，不能一包一睡）
        let due = sent + rate_bps / 8.0 * 0.004;
        while sent < due {
            probe(PKT);
            sent += PKT as f64;
        }
        std::thread::sleep(Duration::from_millis(4));
    }
    std::thread::sleep(Duration::from_millis(400));
    control(false);
    let t = Instant::now();
    let res = loop {
        if let Some(r) = SERVER_RESULT.lock().unwrap().take() {
            break Some(r);
        }
        if t.elapsed() > Duration::from_secs(3) {
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    match res {
        Some(v) if v.is_empty() => row.note = "30 秒内只能测一次，稍后再试".into(),
        Some(mut v) => {
            v.sort_unstable_by(|a, b| b.cmp(a));
            let top: Vec<u32> = v.into_iter().take(2).collect();
            if top.is_empty() || top[0] == 0 {
                row.note = "服务器没收到测速包（UDP 不通？）".into();
            } else {
                row.mbps = Some(top.iter().sum::<u32>() as f32 / top.len() as f32 / 1000.0);
            }
        }
        None => row.note = "服务器没回结果".into(),
    }
    row
}

fn agent(timeout: Duration) -> ureq::Agent {
    use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
    // 用系统的根证书（内置的那份不全：Cloudflare 的证书链它认不出）
    ureq::Agent::config_builder()
        .tls_config(TlsConfig::builder().provider(TlsProvider::NativeTls).root_certs(RootCerts::PlatformVerifier).build())
        .timeout_global(Some(timeout))
        .user_agent("VOC")
        .build()
        .new_agent()
}

/// Cloudflare：先测几次空请求的往返（取最小），再上传 1 / 4 / 10 / 25 MB，单次超过 3 秒就停，取最快的
fn test_cloudflare() -> Row {
    let mut row = Row { name: "Cloudflare".into(), ..Default::default() };
    let a = agent(Duration::from_secs(10));
    let mut lat = u32::MAX;
    let mut err = String::new();
    for _ in 0..4 {
        let t = Instant::now();
        match a.get("https://speed.cloudflare.com/__down?bytes=0").call() {
            Ok(_) => lat = lat.min(t.elapsed().as_millis() as u32),
            Err(e) => err = e.to_string(),
        }
    }
    if lat == u32::MAX {
        row.note = format!("连不上（{err}）");
        return row;
    }
    row.latency_ms = Some(lat);
    let mut best = 0f32;
    for mb in [1usize, 4, 10, 25] {
        let body = vec![0u8; mb * 1_000_000];
        let t = Instant::now();
        if a.post("https://speed.cloudflare.com/__up").header("Content-Type", "application/octet-stream").send(&body[..]).is_err() {
            break;
        }
        // 扣掉一个往返（建连 / 等回复）
        let secs = (t.elapsed().as_secs_f32() - lat as f32 / 1000.0).max(0.05);
        best = best.max(body.len() as f32 * 8.0 / secs / 1e6);
        if t.elapsed() > Duration::from_secs(3) {
            break;
        }
    }
    if best > 0.0 {
        row.mbps = Some(best);
    } else {
        row.note = "上传失败".into();
    }
    row
}

// ---------------------------------------------------------------- iperf3（标准协议，上传 4 秒）

const IPERF_TEST_START: u8 = 1;
const IPERF_TEST_RUNNING: u8 = 2;
const IPERF_TEST_END: u8 = 4;
const IPERF_PARAM_EXCHANGE: u8 = 9;
const IPERF_CREATE_STREAMS: u8 = 10;
const IPERF_EXCHANGE_RESULTS: u8 = 13;
const IPERF_DISPLAY_RESULTS: u8 = 14;
const IPERF_DONE: u8 = 16;
const IPERF_ACCESS_DENIED: u8 = 0xFF;

fn test_iperf3(name: &str, host: &str, ports: &[u16]) -> Row {
    let mut row = Row { name: name.into(), ..Default::default() };
    let mut last = String::from("连不上");
    for &port in ports {
        match iperf3_upload(host, port, 4) {
            Ok((lat, mbps)) => {
                row.latency_ms = Some(lat);
                row.mbps = Some(mbps);
                return row;
            }
            Err(e) => last = e,
        }
    }
    row.note = last;
    row
}

fn read_state(c: &mut TcpStream) -> Result<u8, String> {
    let mut b = [0u8; 1];
    c.read_exact(&mut b).map_err(|_| "连接断了".to_string())?;
    Ok(b[0])
}

fn send_json(c: &mut TcpStream, v: &serde_json::Value) -> Result<(), String> {
    let s = v.to_string();
    c.write_all(&(s.len() as u32).to_be_bytes()).and_then(|_| c.write_all(s.as_bytes())).map_err(|e| e.to_string())
}

fn read_json(c: &mut TcpStream) -> Result<serde_json::Value, String> {
    let mut l = [0u8; 4];
    c.read_exact(&mut l).map_err(|e| e.to_string())?;
    let n = u32::from_be_bytes(l) as usize;
    if n > 1 << 20 {
        return Err("回复太大".into());
    }
    let mut b = vec![0u8; n];
    c.read_exact(&mut b).map_err(|e| e.to_string())?;
    serde_json::from_slice(&b).map_err(|e| e.to_string())
}

/// 返回 (建连用时 ms, 上行 Mbps)
fn iperf3_upload(host: &str, port: u16, secs: u64) -> Result<(u32, f32), String> {
    let addr = (host, port).to_socket_addrs().map_err(|_| "解析不了地址".to_string())?.next().ok_or("解析不了地址")?;
    let t = Instant::now();
    let mut c = TcpStream::connect_timeout(&addr, Duration::from_secs(4)).map_err(|_| "连不上".to_string())?;
    let lat = t.elapsed().as_millis() as u32;
    c.set_read_timeout(Some(Duration::from_secs(6))).ok();
    c.set_write_timeout(Some(Duration::from_secs(6))).ok();
    // 36 个字符的随机 cookie + '\0'
    let mut cookie = [0u8; 37];
    let mut x = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1)) | 1;
    for b in cookie.iter_mut().take(36) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = b"abcdefghijklmnopqrstuvwxyz234567"[(x % 32) as usize];
    }
    c.write_all(&cookie).map_err(|e| e.to_string())?;
    let mut sent = 0u64;
    loop {
        match read_state(&mut c)? {
            IPERF_PARAM_EXCHANGE => send_json(
                &mut c,
                &serde_json::json!({"tcp": true, "omit": 0, "time": secs, "num": 0, "blockcount": 0, "parallel": 1, "len": 131072, "pacing_timer": 1000, "client_version": "3.9"}),
            )?,
            IPERF_CREATE_STREAMS => {
                let mut d = TcpStream::connect_timeout(&addr, Duration::from_secs(4)).map_err(|_| "数据连接连不上".to_string())?;
                d.set_write_timeout(Some(Duration::from_secs(2))).ok();
                d.write_all(&cookie).map_err(|e| e.to_string())?;
                // 等服务器说开始
                loop {
                    match read_state(&mut c)? {
                        IPERF_TEST_START => continue,
                        IPERF_TEST_RUNNING => break,
                        IPERF_ACCESS_DENIED => return Err("节点忙（一次只让一个人测）".into()),
                        s => return Err(format!("节点回了意外的状态 {s}")),
                    }
                }
                let buf = vec![0u8; 131072];
                let t0 = Instant::now();
                while t0.elapsed() < Duration::from_secs(secs) {
                    match d.write(&buf) {
                        Ok(n) => sent += n as u64,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {}
                        Err(_) => break,
                    }
                }
                c.write_all(&[IPERF_TEST_END]).map_err(|e| e.to_string())?;
                drop(d);
            }
            IPERF_EXCHANGE_RESULTS => {
                send_json(
                    &mut c,
                    &serde_json::json!({"cpu_util_total": 0, "cpu_util_user": 0, "cpu_util_system": 0, "sender_has_retransmits": 0,
                        "streams": [{"id": 1, "bytes": sent, "retransmits": 0, "jitter": 0, "errors": 0, "packets": 0, "start_time": 0, "end_time": secs as f64}]}),
                )?;
                let r = read_json(&mut c)?;
                // 服务器那边收到的字节数才是真的上行
                let got = r["streams"][0]["bytes"].as_u64().unwrap_or(0);
                let end = r["streams"][0]["end_time"].as_f64().filter(|t| *t > 0.5).unwrap_or(secs as f64);
                let _ = read_state(&mut c); // DISPLAY_RESULTS
                let _ = c.write_all(&[IPERF_DONE]);
                if got == 0 {
                    return Err("节点没收到数据".into());
                }
                return Ok((lat, (got as f64 * 8.0 / end / 1e6) as f32));
            }
            IPERF_DISPLAY_RESULTS => {}
            IPERF_ACCESS_DENIED => return Err("节点忙（一次只让一个人测）".into()),
            s if s >= 0xFD => return Err("节点出错".into()),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outliers_dropped() {
        let (avg, d) = robust_average(&[50.0, 48.0, 10.0]).unwrap();
        assert_eq!(d, vec![false, false, true]);
        assert!((avg - 49.0).abs() < 0.01);
        let (avg, d) = robust_average(&[20.0]).unwrap();
        assert_eq!((avg, d), (20.0, vec![false]));
        assert!(robust_average(&[]).is_none());
        // 高的不去掉（只去离谱的低值）
        let (_, d) = robust_average(&[10.0, 11.0, 90.0]).unwrap();
        assert_eq!(d, vec![false, false, false]);
    }
}
