//! TCP 控制连接：抗 DPI 握手（ML-KEM-768 + 授权码）后，全部走加密帧。

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;
use voc_proto::auth::AuthCode;
use voc_proto::crypto::*;
use voc_proto::{ClientMsg, ServerMsg, DEFAULT_PORT, PROTOCOL_VERSION};

pub enum NetEvent {
    Msg(ServerMsg),
    /// 图片分块（服务器转来的密文，file::read_server_chunk 格式）
    File(Vec<u8>),
    Closed(String),
}

pub enum Outgoing {
    Msg(ClientMsg),
    /// UDP 不可用时的语音（udp::client_voice 格式）
    Voice(Vec<u8>),
    /// UDP 不可用时的视频分片
    Video(Vec<u8>),
    /// 图片分块（file::client_chunk 格式）
    File(Vec<u8>),
}

pub struct Conn {
    tx: Sender<Outgoing>,
    stream: TcpStream,
    pub server_addr: SocketAddr,
    pub keys: SessionKeys,
}

impl Conn {
    pub fn send(&self, msg: ClientMsg) {
        let _ = self.tx.send(Outgoing::Msg(msg));
    }
    pub fn sender(&self) -> Sender<Outgoing> {
        self.tx.clone()
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

pub fn resolve(addr: &str) -> Result<SocketAddr, String> {
    let addr = addr.trim();
    if addr.is_empty() {
        return Err("请输入服务器地址".into());
    }
    let with_port = if addr.parse::<SocketAddr>().is_ok()
        || (addr.contains(':') && !addr.contains("::") && !addr.ends_with(']'))
    {
        addr.to_string()
    } else if addr.starts_with('[') {
        format!("{addr}:{DEFAULT_PORT}")
    } else if addr.contains("::") {
        format!("[{addr}]:{DEFAULT_PORT}")
    } else {
        format!("{addr}:{DEFAULT_PORT}")
    };
    with_port
        .to_socket_addrs()
        .map_err(|e| format!("无法解析地址 {with_port}：{e}"))?
        .next()
        .ok_or_else(|| format!("无法解析地址 {with_port}"))
}

const NO_ANSWER: &str = "服务器没有响应：授权码错误或已失效、地址不对，或本机时间偏差超过 10 分钟";

fn read_exact_or(stream: &mut TcpStream, buf: &mut [u8]) -> Result<(), String> {
    stream.read_exact(buf).map_err(|_| NO_ANSWER.to_string())
}

fn read_frame(r: &mut TcpStream, op: &mut FrameOpener) -> Option<(u8, Vec<u8>)> {
    let mut h = [0u8; FRAME_LEN_LEN];
    r.read_exact(&mut h).ok()?;
    let n = op.open_len(&h)?;
    let mut b = vec![0u8; n];
    r.read_exact(&mut b).ok()?;
    op.open_body(&b)
}

/// 阻塞连接（请在后台线程调用）。
/// - `notify`：收到新事件时调用（唤醒 UI）
/// - `on_media`：收到 TCP 隧道里的语音 / 视频时**在读线程里直接调用**（kind, 数据），不经过 UI，保证低延迟
pub fn connect(
    addr: &str,
    name: &str,
    code: &str,
    notify: impl Fn() + Send + 'static,
    mut on_media: impl FnMut(u8, &[u8]) + Send + 'static,
) -> Result<(Conn, Receiver<NetEvent>), String> {
    let code = AuthCode::parse(code)?;
    let server_addr = resolve(addr)?;
    let mut stream = TcpStream::connect_timeout(&server_addr, Duration::from_secs(5))
        .map_err(|e| format!("连接 {server_addr} 失败：{e}"))?;
    let _ = stream.set_nodelay(true);
    stream.set_read_timeout(Some(Duration::from_secs(8))).map_err(|e| e.to_string())?;

    // ---- 握手 ----
    let mut hs = ClientHandshake::start(code.psk());
    stream.write_all(hs.first_flight()).map_err(|e| e.to_string())?;
    let mut prefix = [0u8; SERVER_PREFIX_LEN];
    read_exact_or(&mut stream, &mut prefix)?;
    let need = hs.read_server_prefix(&prefix).map_err(|_| NO_ANSWER.to_string())?;
    let mut rest = vec![0u8; need];
    read_exact_or(&mut stream, &mut rest)?;
    let keys = hs.finish(&rest).map_err(|_| NO_ANSWER.to_string())?;

    let mut sealer = FrameSealer::new(&keys.tcp_c2s);
    let mut opener = FrameOpener::new(&keys.tcp_s2c);
    let hello = ClientMsg::Hello { name: name.trim().to_string(), version: PROTOCOL_VERSION };
    stream
        .write_all(&sealer.seal(KIND_JSON, serde_json::to_string(&hello).unwrap().as_bytes()))
        .map_err(|e| e.to_string())?;
    // 等第一条回复（Welcome 或 Error），确认登录成功
    let first = read_frame(&mut stream, &mut opener).ok_or("登录失败：服务器断开了连接")?;
    let first: ServerMsg = serde_json::from_slice(&first.1).map_err(|_| "服务器回复格式错误")?;
    if let ServerMsg::Error { message } = first {
        return Err(message);
    }
    stream.set_read_timeout(None).map_err(|e| e.to_string())?;

    let (evt_tx, evt_rx) = mpsc::channel();
    let _ = evt_tx.send(NetEvent::Msg(first));
    let (tx, rx) = mpsc::channel::<Outgoing>();

    // 写线程
    let mut w = stream.try_clone().map_err(|e| e.to_string())?;
    std::thread::Builder::new()
        .name("tcp-write".into())
        .spawn(move || {
            for o in rx {
                let f = match o {
                    Outgoing::Msg(m) => sealer.seal(KIND_JSON, serde_json::to_string(&m).unwrap().as_bytes()),
                    Outgoing::Voice(v) => sealer.seal(KIND_VOICE, &v),
                    Outgoing::Video(v) => sealer.seal(KIND_VIDEO, &v),
                    Outgoing::File(v) => sealer.seal(KIND_FILE, &v),
                };
                if w.write_all(&f).is_err() {
                    break;
                }
            }
        })
        .map_err(|e| e.to_string())?;

    // 读线程
    let mut r = stream.try_clone().map_err(|e| e.to_string())?;
    std::thread::Builder::new()
        .name("tcp-read".into())
        .spawn(move || {
            while let Some((kind, data)) = read_frame(&mut r, &mut opener) {
                match kind {
                    KIND_VOICE | KIND_VIDEO => on_media(kind, &data),
                    KIND_FILE => {
                        if evt_tx.send(NetEvent::File(data)).is_err() {
                            return;
                        }
                        notify();
                    }
                    KIND_JSON => {
                        if let Ok(msg) = serde_json::from_slice::<ServerMsg>(&data) {
                            if evt_tx.send(NetEvent::Msg(msg)).is_err() {
                                return;
                            }
                            notify();
                        }
                    }
                    _ => {}
                }
            }
            let _ = evt_tx.send(NetEvent::Closed("与服务器的连接断开了".into()));
            notify();
        })
        .map_err(|e| e.to_string())?;

    Ok((Conn { tx, stream, server_addr, keys }, evt_rx))
}

#[cfg(test)]
mod tests {
    use super::resolve;

    #[test]
    fn default_port() {
        assert_eq!(resolve("127.0.0.1").unwrap().port(), 7878);
        assert_eq!(resolve("127.0.0.1:9000").unwrap().port(), 9000);
        assert_eq!(resolve("::1").unwrap().port(), 7878);
        assert_eq!(resolve("[::1]:9000").unwrap().port(), 9000);
        assert_eq!(resolve("localhost").unwrap().port(), 7878);
    }
}
