//! 客户端端到端加密管理：身份验证、成对密钥、语音发送者密钥轮换、聊天加解密。
//! 协议细节见 `voc_proto::e2e`。
//!
//! 共享方式：`Arc<Mutex<E2e>>`。界面线程处理信令并取走待发消息（[`E2e::take_outgoing`]），
//! 编码线程加密语音帧，接收线程解密语音帧。

use std::collections::HashMap;
use voc_proto::e2e::{self, b64, unb64, Identity, ReceiverKeyring, SenderKey};
use voc_proto::p2p::{self, LocalKem, PairKeys};
use voc_proto::{ClientMsg, E2eEntry};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Trust {
    /// 第一次见到，已自动记住
    New,
    /// 与记住的身份一致
    Known,
    /// 身份和以前记住的不一样：可能换了电脑，也可能有人（包括服务器）在冒充。暂停加密通信。
    Changed,
}

pub struct Member {
    pub name: String,
    pub idk: Vec<u8>,
    ek: Vec<u8>,
    pub trust: Trust,
    pub safety: String,
}

pub struct E2e {
    pub my_id: u32,
    my_name: String,
    identity: Identity,
    kem: LocalKem,
    pub members: HashMap<u32, Member>,
    pairs: HashMap<u32, PairKeys>,
    /// 对方 PeerKey 先于身份公告到达时暂存
    pending_ct: HashMap<u32, (Vec<u8>, Vec<u8>)>,
    sender: SenderKey,
    sent_epoch: HashMap<u32, u16>,
    channel_others: Vec<u32>,
    keyring: ReceiverKeyring,
    /// 昵称 → 身份指纹（这台服务器上记住的）
    pub pins: HashMap<String, String>,
    pub pins_dirty: bool,
    out: Vec<ClientMsg>,
}

pub struct ChatIn {
    pub from: u32,
    pub text: String,
}

/// 解开的端到端信封
pub enum BlobIn {
    Chat(ChatIn),
    /// 别人发来的图片密钥和信息（图片本身随后分块到）
    Image { from: u32, meta: voc_proto::file::ImageMeta },
}

impl E2e {
    pub fn new(my_id: u32, my_name: &str, identity: Identity, pins: HashMap<String, String>) -> Self {
        let mut s = Self {
            my_id,
            my_name: my_name.to_string(),
            identity,
            kem: LocalKem::default(),
            members: HashMap::new(),
            pairs: HashMap::new(),
            pending_ct: HashMap::new(),
            sender: SenderKey::new(rand_epoch()),
            sent_epoch: HashMap::new(),
            channel_others: Vec::new(),
            keyring: ReceiverKeyring::default(),
            pins,
            pins_dirty: false,
            out: Vec::new(),
        };
        let sig = s.identity.sign_ek(&s.my_name, &s.kem.ek);
        s.out.push(ClientMsg::E2eAnnounce {
            entry: E2eEntry {
                id: my_id,
                name: s.my_name.clone(),
                idk: b64(&s.identity.public),
                ek: b64(&s.kem.ek),
                sig: b64(&sig),
            },
        });
        s
    }

    pub fn take_outgoing(&mut self) -> Vec<ClientMsg> {
        std::mem::take(&mut self.out)
    }

    pub fn has_pair(&self, id: u32) -> bool {
        self.pairs.contains_key(&id)
    }

    /// P2P 直连用的成对密钥（复制一份）
    pub fn pair_keys(&self, id: u32) -> Option<PairKeys> {
        self.pairs.get(&id).map(|k| PairKeys { send: k.send, send_hp: k.send_hp, recv: k.recv, recv_hp: k.recv_hp, blob: k.blob })
    }

    fn trusted(&self, id: u32) -> bool {
        self.members.get(&id).is_some_and(|m| m.trust != Trust::Changed)
    }

    /// 收到身份公告（目录 / 新加入）。返回 Err(原因) 表示公告无效（签名不对）。
    pub fn on_entry(&mut self, e: E2eEntry) -> Result<Trust, String> {
        if e.id == self.my_id {
            return Ok(Trust::Known);
        }
        let (Some(idk), Some(ek), Some(sig)) = (unb64(&e.idk), unb64(&e.ek), unb64(&e.sig)) else {
            return Err(format!("{} 的加密身份格式错误", e.name));
        };
        if !e2e::verify_ek(&idk, &e.name, &ek, &sig) {
            return Err(format!("{} 的加密身份签名无效（可能被篡改），已忽略", e.name));
        }
        let fp = e2e::fingerprint(&idk);
        let trust = match self.pins.get(&e.name) {
            None => {
                self.pins.insert(e.name.clone(), fp);
                self.pins_dirty = true;
                Trust::New
            }
            Some(p) if *p == fp => Trust::Known,
            Some(_) => Trust::Changed,
        };
        // 重新公告（换了会话密钥）就作废旧的成对密钥
        self.pairs.remove(&e.id);
        self.sent_epoch.remove(&e.id);
        let safety = e2e::safety_code(&idk);
        self.members.insert(e.id, Member { name: e.name, idk, ek, trust, safety });
        if trust != Trust::Changed {
            self.start_pair(e.id);
        }
        Ok(trust)
    }

    fn start_pair(&mut self, id: u32) {
        if self.my_id < id {
            // id 小的一方发起：封装 + 签名
            let Some(m) = self.members.get(&id) else { return };
            let Some((ct, ss)) = p2p::encapsulate(&m.ek) else { return };
            let sig = self.identity.sign_pair(&m.ek, &ct);
            let k = p2p::pair_keys(&ss, self.my_id, &self.kem.ek, id, &m.ek);
            self.out.push(ClientMsg::PeerKey { to: id, ct: b64(&ct), sig: b64(&sig) });
            self.pairs.insert(id, k);
            self.flush_sender_keys();
        } else if let Some((ct, sig)) = self.pending_ct.remove(&id) {
            self.accept_ct(id, &ct, &sig);
        }
    }

    fn accept_ct(&mut self, from: u32, ct: &[u8], sig: &[u8]) -> bool {
        let Some(m) = self.members.get(&from) else { return false };
        if m.trust == Trust::Changed || !e2e::verify_pair(&m.idk, &self.kem.ek, ct, sig) {
            return false;
        }
        let Some(ss) = self.kem.decapsulate(ct) else { return false };
        let k = p2p::pair_keys(&ss, self.my_id, &self.kem.ek, from, &m.ek);
        self.pairs.insert(from, k);
        self.flush_sender_keys();
        true
    }

    pub fn on_peer_key(&mut self, from: u32, ct: &str, sig: &str) {
        let (Some(ct), Some(sig)) = (unb64(ct), unb64(sig)) else { return };
        if from >= self.my_id {
            return; // 只接受 id 比自己小的一方发起
        }
        if self.members.contains_key(&from) {
            self.accept_ct(from, &ct, &sig);
        } else {
            self.pending_ct.insert(from, (ct, sig));
        }
    }

    /// 用户核对安全码后，接受某人的新身份
    pub fn trust(&mut self, id: u32) {
        let Some(m) = self.members.get_mut(&id) else { return };
        self.pins.insert(m.name.clone(), e2e::fingerprint(&m.idk));
        self.pins_dirty = true;
        m.trust = Trust::Known;
        self.start_pair(id);
    }

    /// 只保留在线的人
    pub fn retain_online(&mut self, online: &[u32]) {
        self.members.retain(|id, _| online.contains(id));
        self.pairs.retain(|id, _| online.contains(id));
        self.keyring.by_sender.retain(|id, _| online.contains(id));
        self.pending_ct.retain(|id, _| online.contains(id));
    }

    /// 同频道成员变化：换新语音密钥并发给现在的成员（离开的人拿不到新密钥）
    pub fn set_channel_others(&mut self, mut ids: Vec<u32>) {
        ids.sort();
        if ids != self.channel_others {
            self.channel_others = ids;
            self.rotate();
        }
    }

    fn rotate(&mut self) {
        self.sender = SenderKey::new(self.sender.epoch.wrapping_add(1));
        self.sent_epoch.clear();
        self.flush_sender_keys();
    }

    /// 把当前语音密钥发给还没拿到的同频道成员
    pub fn flush_sender_keys(&mut self) {
        let export = self.sender.export();
        for &id in &self.channel_others.clone() {
            if self.sent_epoch.get(&id) == Some(&self.sender.epoch) || !self.trusted(id) {
                continue;
            }
            let Some(k) = self.pairs.get(&id) else { continue };
            let blob = e2e::seal_blob(&k.blob, e2e::BLOB_SENDER_KEY, &export);
            self.out.push(ClientMsg::E2eBlob { to: id, blob: b64(&blob) });
            self.sent_epoch.insert(id, self.sender.epoch);
        }
    }

    /// 收到加密信封。聊天消息返回给界面。
    pub fn on_blob(&mut self, from: u32, blob: &str) -> Option<BlobIn> {
        let blob = unb64(blob)?;
        if !self.trusted(from) {
            return None;
        }
        let (kind, data) = e2e::open_blob(&self.pairs.get(&from)?.blob, &blob)?;
        match kind {
            e2e::BLOB_SENDER_KEY => {
                self.keyring.by_sender.entry(from).or_default().install(&data);
                None
            }
            e2e::BLOB_CHAT => Some(BlobIn::Chat(ChatIn { from, text: String::from_utf8_lossy(&data).into_owned() })),
            e2e::BLOB_IMAGE => {
                let meta: voc_proto::file::ImageMeta = serde_json::from_slice(&data).ok()?;
                (meta.size as usize <= voc_proto::file::MAX_IMAGE_BYTES && meta.chunks <= voc_proto::file::MAX_CHUNKS)
                    .then_some(BlobIn::Image { from, meta })
            }
            _ => None,
        }
    }

    /// 给所有在线、已建立加密的成员发聊天。返回（送达人数，暂时收不到的人的昵称）。
    pub fn send_chat(&mut self, text: &str) -> (usize, Vec<String>) {
        let mut sent = 0;
        let mut missing = Vec::new();
        let ids: Vec<u32> = self.members.keys().copied().collect();
        for id in ids {
            match (self.trusted(id), self.pairs.get(&id)) {
                (true, Some(k)) => {
                    let blob = e2e::seal_blob(&k.blob, e2e::BLOB_CHAT, text.as_bytes());
                    self.out.push(ClientMsg::E2eBlob { to: id, blob: b64(&blob) });
                    sent += 1;
                }
                _ => missing.push(self.members[&id].name.clone()),
            }
        }
        (sent, missing)
    }

    /// 把图片密钥和信息发给指定的人（同频道成员）。返回收不到的人的名字。
    pub fn send_image_meta(&mut self, to: &[u32], meta: &voc_proto::file::ImageMeta) -> Vec<String> {
        let json = serde_json::to_vec(meta).expect("meta json");
        let mut missing = Vec::new();
        for &id in to {
            match (self.trusted(id), self.pairs.get(&id)) {
                (true, Some(k)) => {
                    let blob = e2e::seal_blob(&k.blob, e2e::BLOB_IMAGE, &json);
                    self.out.push(ClientMsg::E2eBlob { to: id, blob: b64(&blob) });
                }
                _ => missing.push(self.members.get(&id).map(|m| m.name.clone()).unwrap_or_else(|| format!("#{id}"))),
            }
        }
        missing
    }

    /// 加密自己的一帧语音
    pub fn encrypt_frame(&mut self, frame: &[u8]) -> Vec<u8> {
        if self.sender.exhausted() {
            self.rotate();
        }
        self.sender.encrypt(self.my_id, frame)
    }

    /// 加密自己的一整帧视频
    pub fn encrypt_video(&mut self, frame: &[u8]) -> Vec<u8> {
        self.sender.encrypt_video(self.my_id, frame)
    }

    /// 解密别人的一整帧视频
    pub fn decrypt_video(&mut self, from: u32, blob: &[u8]) -> Option<Vec<u8>> {
        self.keyring.by_sender.get_mut(&from)?.decrypt_video(from, blob)
    }

    /// 解密别人的一帧语音，返回 (帧计数器, Opus)
    pub fn decrypt_frame(&mut self, from: u32, blob: &[u8]) -> Option<(u32, Vec<u8>)> {
        self.keyring.by_sender.get_mut(&from)?.decrypt(from, blob)
    }

}

fn rand_epoch() -> u16 {
    u16::from_le_bytes(voc_proto::auth::random_bytes::<2>())
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use voc_proto::ServerMsg;

    /// 模拟服务器：把 a 的待发消息转交给 b（只处理 E2E 相关的）
    pub fn relay(from: &mut E2e, to: &mut E2e) -> Vec<ChatIn> {
        let mut chats = Vec::new();
        for m in from.take_outgoing() {
            let msg = match m {
                ClientMsg::E2eAnnounce { entry } => ServerMsg::E2eJoin { entry },
                ClientMsg::PeerKey { to: t, ct, sig } if t == to.my_id => ServerMsg::PeerKey { from: from.my_id, ct, sig },
                ClientMsg::E2eBlob { to: t, blob } if t == to.my_id => ServerMsg::E2eBlob { from: from.my_id, blob, ts: 0 },
                _ => continue,
            };
            match msg {
                ServerMsg::E2eJoin { entry } => {
                    to.on_entry(entry).unwrap();
                }
                ServerMsg::PeerKey { from, ct, sig } => to.on_peer_key(from, &ct, &sig),
                ServerMsg::E2eBlob { from, blob, .. } => {
                    if let Some(super::BlobIn::Chat(c)) = to.on_blob(from, &blob) {
                        chats.push(c);
                    }
                }
                _ => {}
            }
        }
        chats
    }

    /// 模拟服务器：多方之间按收件人投递，直到没有待发消息（三人以上的测试用）
    pub fn route_all(es: &mut [E2e]) {
        for _ in 0..8 {
            let mut mail: Vec<(u32, ClientMsg)> = Vec::new();
            for e in es.iter_mut() {
                let me = e.my_id;
                mail.extend(e.take_outgoing().into_iter().map(|m| (me, m)));
            }
            if mail.is_empty() {
                return;
            }
            for (from, m) in mail {
                for to in es.iter_mut().filter(|e| e.my_id != from) {
                    match &m {
                        ClientMsg::E2eAnnounce { entry } => {
                            let _ = to.on_entry(entry.clone());
                        }
                        ClientMsg::PeerKey { to: t, ct, sig } if *t == to.my_id => to.on_peer_key(from, ct, sig),
                        ClientMsg::E2eBlob { to: t, blob } if *t == to.my_id => {
                            to.on_blob(from, blob);
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    pub fn pair_up(a: &mut E2e, b: &mut E2e) {
        for _ in 0..3 {
            relay(a, b);
            relay(b, a);
        }
    }

    #[test]
    fn voice_and_chat_end_to_end() {
        let mut a = E2e::new(1, "alice", Identity::generate(), HashMap::new());
        let mut b = E2e::new(2, "bob", Identity::generate(), HashMap::new());
        pair_up(&mut a, &mut b);
        assert!(a.has_pair(2) && b.has_pair(1));
        assert_eq!(b.members[&1].trust, Trust::New);

        a.set_channel_others(vec![2]);
        b.set_channel_others(vec![1]);
        pair_up(&mut a, &mut b);
        let f = a.encrypt_frame(b"opus");
        assert_eq!(b.decrypt_frame(1, &f).map(|x| x.1), Some(b"opus".to_vec()));

        a.send_chat("你好");
        let chats = relay(&mut a, &mut b);
        assert_eq!(chats.len(), 1);
        assert_eq!(chats[0].text, "你好");

        // 有人离开频道：alice 换密钥，bob 的旧密钥解不开新语音
        a.set_channel_others(vec![]);
        let f2 = a.encrypt_frame(b"secret");
        assert!(b.decrypt_frame(1, &f2).is_none(), "left member must not decrypt new frames");
    }

    #[test]
    fn changed_identity_is_flagged_and_blocked() {
        let mut a = E2e::new(1, "alice", Identity::generate(), HashMap::new());
        let mut b = E2e::new(2, "bob", Identity::generate(), HashMap::new());
        pair_up(&mut a, &mut b);
        let pins = b.pins.clone();
        assert!(pins.contains_key("alice"));

        // 下次登录，「alice」带着另一个身份出现（换了电脑，或服务器冒充）
        let mut fake = E2e::new(1, "alice", Identity::generate(), HashMap::new());
        let mut b2 = E2e::new(2, "bob", Identity::generate(), pins);
        relay(&mut b2, &mut fake);
        let entry = match fake.take_outgoing().remove(0) {
            ClientMsg::E2eAnnounce { entry } => entry,
            _ => unreachable!(),
        };
        assert_eq!(b2.on_entry(entry).unwrap(), Trust::Changed);
        assert!(!b2.has_pair(1), "no keys exchanged with an unverified identity");
        b2.set_channel_others(vec![1]);
        assert!(b2.take_outgoing().iter().all(|m| !matches!(m, ClientMsg::E2eBlob { .. })), "no sender key to changed identity");
        // 核对安全码后手动信任
        b2.trust(1);
        assert!(!b2.pins_dirty || b2.pins["alice"] == e2e::fingerprint(&b2.members[&1].idk));
    }

    #[test]
    fn forged_entries_rejected() {
        let mut a = E2e::new(1, "alice", Identity::generate(), HashMap::new());
        let mut entry = match a.take_outgoing().remove(0) {
            ClientMsg::E2eAnnounce { entry } => entry,
            _ => unreachable!(),
        };
        let mut b = E2e::new(2, "bob", Identity::generate(), HashMap::new());
        // 服务器把 alice 的公告改名成 carol
        entry.name = "carol".into();
        assert!(b.on_entry(entry.clone()).is_err());
        // 服务器换掉会话公钥（想做中间人）
        entry.name = "alice".into();
        entry.ek = b64(&LocalKem::default().ek);
        assert!(b.on_entry(entry).is_err());
        assert!(b.members.is_empty());
    }

    #[test]
    fn forged_peer_key_rejected() {
        let mut a = E2e::new(1, "alice", Identity::generate(), HashMap::new());
        let mut b = E2e::new(2, "bob", Identity::generate(), HashMap::new());
        relay(&mut a, &mut b); // bob 认识 alice
        relay(&mut b, &mut a); // alice 认识 bob，会发 PeerKey
        a.take_outgoing(); // 丢弃 alice 真正的 PeerKey
        // 服务器自己封装一份冒充 alice
        let (ct, _) = p2p::encapsulate(&b.kem.ek).unwrap();
        let mallory = Identity::generate();
        let sig = mallory.sign_pair(&b.kem.ek, &ct);
        b.on_peer_key(1, &b64(&ct), &b64(&sig));
        assert!(!b.has_pair(1), "pair key with forged signature must be rejected");
    }
}
