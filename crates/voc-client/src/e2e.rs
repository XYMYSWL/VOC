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
    /// 对方公告的能力（BLOB_HELLO）
    pub features: Vec<String>,
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
    /// 已经发过能力公告的人
    hello_sent: std::collections::HashSet<u32>,
    /// 下一条聊天的编号
    next_msg: u64,
    /// 我的个人资料（端到端发给成员）、成员发来的
    my_profile: e2e::Profile,
    pub profiles: HashMap<u32, e2e::Profile>,
}

pub struct ChatIn {
    pub from: u32,
    pub text: String,
    /// 私聊
    pub dm: bool,
    /// 消息编号（对方是旧版本时没有，不能回应）
    pub id: Option<u64>,
}

/// 解开的端到端信封
pub enum BlobIn {
    Chat(ChatIn),
    /// 别人发来的图片密钥和信息（图片本身随后分块到）
    Image { from: u32, meta: voc_proto::file::ImageMeta, dm: bool },
    /// 表情回应：by 对 (r.from, r.id) 这条消息加上 / 取消 r.emoji
    React { by: u32, r: e2e::React },
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
            hello_sent: Default::default(),
            // 编号从当前时间起，重新登录后也不会和之前的重复
            next_msg: voc_proto::now_millis() << 10,
            my_profile: Default::default(),
            profiles: HashMap::new(),
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
        // 刚建立加密的人：发一次能力公告
        let new: Vec<u32> = self.pairs.keys().copied().filter(|id| self.trusted(*id) && !self.hello_sent.contains(id)).collect();
        if !new.is_empty() {
            let hello = serde_json::to_vec(&e2e::Hello { features: vec![e2e::FEATURE_CHAT2.into()] }).expect("hello json");
            for id in new {
                let blob = e2e::seal_blob(&self.pairs[&id].blob, e2e::BLOB_HELLO, &hello);
                self.out.push(ClientMsg::E2eBlob { to: id, blob: b64(&blob) });
                self.hello_sent.insert(id);
            }
        }
        std::mem::take(&mut self.out)
    }

    /// 对方认识带编号的聊天和表情回应
    pub fn supports_chat2(&self, id: u32) -> bool {
        self.members.get(&id).is_some_and(|m| m.features.iter().any(|f| f == e2e::FEATURE_CHAT2))
    }

    /// 一条聊天发给一个人：新版本发带编号的，旧版本发纯文字
    fn seal_chat(&self, id: u32, msg: &e2e::Chat2) -> Option<ClientMsg> {
        let k = self.pairs.get(&id).filter(|_| self.trusted(id))?;
        let blob = if self.supports_chat2(id) {
            e2e::seal_blob(&k.blob, e2e::BLOB_CHAT2, &serde_json::to_vec(msg).expect("chat json"))
        } else {
            e2e::seal_blob(&k.blob, if msg.dm { e2e::BLOB_DM } else { e2e::BLOB_CHAT }, msg.text.as_bytes())
        };
        Some(ClientMsg::E2eBlob { to: id, blob: b64(&blob) })
    }

    fn new_msg_id(&mut self) -> u64 {
        self.next_msg += 1;
        self.next_msg
    }

    /// 更新我的个人资料，发给所有认识的新版本成员
    pub fn set_profile(&mut self, p: e2e::Profile) {
        self.my_profile = p;
        let ids: Vec<u32> = self.members.keys().copied().collect();
        self.send_profile_to(&ids);
    }

    fn send_profile_to(&mut self, to: &[u32]) {
        let json = serde_json::to_vec(&self.my_profile).expect("profile json");
        for &id in to {
            if !(self.supports_chat2(id) && self.trusted(id)) {
                continue;
            }
            if let Some(k) = self.pairs.get(&id) {
                let blob = e2e::seal_blob(&k.blob, e2e::BLOB_PROFILE, &json);
                self.out.push(ClientMsg::E2eBlob { to: id, blob: b64(&blob) });
            }
        }
    }

    /// 表情回应：发给 to 里认识回应的人
    pub fn send_react(&mut self, to: &[u32], r: &e2e::React) {
        let json = serde_json::to_vec(r).expect("react json");
        for &id in to {
            if !(self.supports_chat2(id) && self.trusted(id)) {
                continue;
            }
            if let Some(k) = self.pairs.get(&id) {
                let blob = e2e::seal_blob(&k.blob, e2e::BLOB_REACT, &json);
                self.out.push(ClientMsg::E2eBlob { to: id, blob: b64(&blob) });
            }
        }
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
        self.members.insert(e.id, Member { name: e.name, idk, ek, trust, safety, features: Vec::new() });
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

    /// 诊断用：(信任状态, 成对密钥已建立, 收到了他的语音密钥, 我当前的语音密钥已发给他)
    pub fn member_status(&self, id: u32) -> Option<(Trust, bool, bool, bool)> {
        let m = self.members.get(&id)?;
        Some((
            m.trust,
            self.pairs.contains_key(&id),
            self.keyring.by_sender.get(&id).is_some_and(|k| k.has_any()),
            self.sent_epoch.get(&id) == Some(&self.sender.epoch),
        ))
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
            e2e::BLOB_CHAT | e2e::BLOB_DM => {
                Some(BlobIn::Chat(ChatIn { from, text: String::from_utf8_lossy(&data).into_owned(), dm: kind == e2e::BLOB_DM, id: None }))
            }
            e2e::BLOB_CHAT2 => {
                let c: e2e::Chat2 = serde_json::from_slice(&data).ok()?;
                let text: String = c.text.chars().take(voc_proto::MAX_CHAT_LEN).collect();
                Some(BlobIn::Chat(ChatIn { from, text, dm: c.dm, id: Some(c.id) }))
            }
            e2e::BLOB_HELLO => {
                let h: e2e::Hello = serde_json::from_slice(&data).ok()?;
                if let Some(m) = self.members.get_mut(&from) {
                    m.features = h.features.into_iter().take(16).map(|f| f.chars().take(32).collect()).collect();
                }
                // 新认识的新版本成员：把我的资料也给他
                if self.my_profile != e2e::Profile::default() {
                    self.send_profile_to(&[from]);
                }
                None
            }
            e2e::BLOB_PROFILE => {
                let mut p: e2e::Profile = serde_json::from_slice(&data).ok()?;
                p.music = p.music.map(|m| m.chars().take(64).collect());
                self.profiles.insert(from, p);
                None
            }
            e2e::BLOB_REACT => {
                let r: e2e::React = serde_json::from_slice(&data).ok()?;
                e2e::REACTIONS.contains(&r.emoji.as_str()).then_some(BlobIn::React { by: from, r })
            }
            e2e::BLOB_IMAGE | e2e::BLOB_DM_IMAGE => {
                let meta: voc_proto::file::ImageMeta = serde_json::from_slice(&data).ok()?;
                (meta.size as usize <= voc_proto::file::MAX_IMAGE_BYTES && meta.chunks <= voc_proto::file::MAX_CHUNKS)
                    .then_some(BlobIn::Image { from, meta, dm: kind == e2e::BLOB_DM_IMAGE })
            }
            _ => None,
        }
    }

    /// 给所有在线、已建立加密的成员发聊天。返回（消息编号，送达人数，暂时收不到的人的昵称）。
    pub fn send_chat(&mut self, text: &str) -> (u64, usize, Vec<String>) {
        let msg = e2e::Chat2 { id: self.new_msg_id(), text: text.to_string(), dm: false };
        let mut sent = 0;
        let mut missing = Vec::new();
        let ids: Vec<u32> = self.members.keys().copied().collect();
        for id in ids {
            match self.seal_chat(id, &msg) {
                Some(m) => {
                    self.out.push(m);
                    sent += 1;
                }
                None => missing.push(self.members[&id].name.clone()),
            }
        }
        (msg.id, sent, missing)
    }

    /// 私聊：只发给一个人。None = 和对方的加密还没建立（或身份待核对），没发出；否则返回消息编号
    pub fn send_dm(&mut self, to: u32, text: &str) -> Option<u64> {
        let msg = e2e::Chat2 { id: self.new_msg_id(), text: text.to_string(), dm: true };
        let m = self.seal_chat(to, &msg)?;
        self.out.push(m);
        Some(msg.id)
    }

    /// 把图片密钥和信息发给指定的人（同频道成员，或私聊对象 dm = true）。返回收不到的人的名字。
    pub fn send_image_meta(&mut self, to: &[u32], meta: &voc_proto::file::ImageMeta, dm: bool) -> Vec<String> {
        let json = serde_json::to_vec(meta).expect("meta json");
        let mut missing = Vec::new();
        for &id in to {
            match (self.trusted(id), self.pairs.get(&id)) {
                (true, Some(k)) => {
                    let blob = e2e::seal_blob(&k.blob, if dm { e2e::BLOB_DM_IMAGE } else { e2e::BLOB_IMAGE }, &json);
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

    /// 加密自己的一帧伴奏 / 共享声音（独立的密钥和计数器）
    pub fn encrypt_stream(&mut self, stream: u8, frame: &[u8]) -> Vec<u8> {
        self.sender.encrypt_stream(self.my_id, stream, frame)
    }

    /// 解密别人的一帧伴奏 / 共享声音，返回 (该流的帧计数器, Opus)
    pub fn decrypt_stream(&mut self, from: u32, stream: u8, blob: &[u8]) -> Option<(u32, Vec<u8>)> {
        self.keyring.by_sender.get_mut(&from)?.decrypt_stream(from, stream, blob)
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
        assert!(!chats[0].dm);

        // 私聊：只有对方收得到，并标成私聊
        assert!(a.send_dm(2, "悄悄话").is_some());
        let dms = relay(&mut a, &mut b);
        assert_eq!((dms.len(), dms[0].text.as_str(), dms[0].dm), (1, "悄悄话", true));
        assert!(a.send_dm(99, "没这个人").is_none());

        // 有人离开频道：alice 换密钥，bob 的旧密钥解不开新语音
        a.set_channel_others(vec![]);
        let f2 = a.encrypt_frame(b"secret");
        assert!(b.decrypt_frame(1, &f2).is_none(), "left member must not decrypt new frames");
    }

    /// 新版本之间：聊天带编号、能回应；对方是旧版本（没发能力公告）时退回纯文字，也不发回应
    #[test]
    fn numbered_chat_and_reactions() {
        let mut a = E2e::new(1, "alice", Identity::generate(), HashMap::new());
        let mut b = E2e::new(2, "bob", Identity::generate(), HashMap::new());
        pair_up(&mut a, &mut b);
        assert!(a.supports_chat2(2) && b.supports_chat2(1), "hello exchanged after pairing");

        let (id, sent, _) = a.send_chat("@bob 看这里");
        assert_eq!(sent, 1);
        let chats = relay(&mut a, &mut b);
        assert_eq!((chats[0].id, chats[0].text.as_str()), (Some(id), "@bob 看这里"));

        let r = voc_proto::e2e::React { from: 1, id, emoji: voc_proto::e2e::REACTIONS[0].into(), on: true, dm: false };
        b.send_react(&[1], &r);
        let mut got = None;
        for m in b.take_outgoing() {
            if let ClientMsg::E2eBlob { blob, .. } = m {
                if let Some(BlobIn::React { by, r }) = a.on_blob(2, &blob) {
                    got = Some((by, r));
                }
            }
        }
        assert_eq!(got, Some((2, r)));
        // 不认识的表情忽略
        b.send_react(&[1], &voc_proto::e2e::React { from: 1, id, emoji: "很长的一串".into(), on: true, dm: false });
        for m in b.take_outgoing() {
            if let ClientMsg::E2eBlob { blob, .. } = m {
                assert!(a.on_blob(2, &blob).is_none());
            }
        }

        // bob 是旧版本：没有能力公告 → alice 发纯文字（旧版本能显示），不发回应
        a.members.get_mut(&2).unwrap().features.clear();
        a.send_chat("老版本也能看");
        let chats = relay(&mut a, &mut b);
        assert_eq!((chats[0].id, chats[0].text.as_str()), (None, "老版本也能看"));
        a.send_react(&[2], &voc_proto::e2e::React { from: 2, id: 5, emoji: "👍".into(), on: true, dm: false });
        assert!(a.take_outgoing().is_empty());
    }

    /// 伴奏程序名只端到端给成员；后认识的成员在能力公告后也会收到
    #[test]
    fn profile_reaches_members_end_to_end() {
        let mut a = E2e::new(1, "alice", Identity::generate(), HashMap::new());
        let mut b = E2e::new(2, "bob", Identity::generate(), HashMap::new());
        a.set_profile(voc_proto::e2e::Profile { music: Some("网易云音乐".into()) });
        assert!(a.out.iter().all(|m| !matches!(m, ClientMsg::E2eBlob { .. })), "nobody to send to yet");
        pair_up(&mut a, &mut b);
        relay(&mut b, &mut a); // bob 的能力公告
        relay(&mut a, &mut b); // alice 回给他资料
        assert_eq!(b.profiles.get(&1).and_then(|p| p.music.as_deref()), Some("网易云音乐"));
        a.set_profile(Default::default());
        relay(&mut a, &mut b);
        assert_eq!(b.profiles[&1].music, None);
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
