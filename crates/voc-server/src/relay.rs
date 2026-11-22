//! 成员中继：A、B 之间打不通直连时，让同频道里与两人都直连良好、延迟最低的成员 R 转发语音，
//! 代替服务器中转。
//!
//! 数据来自客户端每 2 秒上报的链路质量（`ClientMsg::LinkStats`）和直连名单（`ClientMsg::DirectPeers`）。
//! 一条路线 A→B 经 R 的条件：
//!   - A 正在直连给 R 发语音（R 收得到 A 的语音包）；
//!   - R 到 A、R 到 B 都是稳定直连（丢包 ≤ 3%，抖动 ≤ 30 ms）；
//!   - R 还有空闲的中继名额（客户端按自己剩余的上行预算算出来）；
//!   - 预计延迟比服务器中转至少快 20 ms。
//! 延迟都按「A 说话到 B 听到再回来」的往返计算，和界面上直连的「⇄ 23 ms」是同一种数，方便对比：
//!   经 R：R–A 往返 + R–B 往返 + 转发开销；经服务器：A–服务器往返 + B–服务器往返。
//! 多个候选里选延迟最低的；已有路线只要仍然有效、且不比最优差太多就保留，避免来回切换。
//! 语音包在 A 端已经端到端加密，R 只是原样转交（R 本来就是频道成员，能听到 A），不泄露额外信息；
//! A 和 B 也不会因此知道对方的 IP。

use std::collections::{HashMap, HashSet};
use voc_proto::RelayRoute;

/// 中继链路允许的最大丢包率（%）
pub const MAX_LOSS: u8 = 3;
/// 中继链路允许的最大抖动（ms）
pub const MAX_JITTER: u32 = 30;
/// 至少比服务器中转快这么多才换成成员中继（往返 ms）
pub const MIN_GAIN_MS: u32 = 20;
/// 中继转发本身的处理开销估算（往返 ms）
pub const FORWARD_COST_MS: u32 = 4;
/// 已有路线比最优差不超过这么多就保留（往返 ms）
pub const KEEP_MARGIN_MS: u32 = 30;

/// 一个频道成员在规划时的输入
#[derive(Clone, Debug, Default)]
pub struct Member {
    pub id: u32,
    pub muted: bool,
    pub deafened: bool,
    /// 到服务器的往返延迟（ms）；没上报（旧版客户端 / 数据过期）为 None
    pub server_rtt: Option<u32>,
    /// 他直连发送语音的对象
    pub direct: HashSet<u32>,
    /// 他测到的稳定直连：对方 id → (往返延迟, 丢包%, 抖动)
    pub links: HashMap<u32, (u32, u8, u32)>,
    /// 还能中转几路
    pub slots: u16,
    /// 他手动选了「走服务器」的人：两人之间不安排成员中继
    pub server_only: HashSet<u32>,
    /// 他手动指定的语音中转（0.6）：对方 → 经谁。两个方向都按这个；安排不了就走服务器（不改用别人）
    pub manual_via: HashMap<u32, u32>,
    /// 他说话时这些人没听到、已改经服务器转发（语音优先）：这段时间不给他们安排中转
    pub no_relay_to: HashSet<u32>,
}

impl Member {
    fn good_link(&self, to: u32) -> Option<u32> {
        let &(rtt, loss, jitter) = self.links.get(&to)?;
        (loss <= MAX_LOSS && jitter <= MAX_JITTER && rtt > 0).then_some(rtt)
    }
}

/// 规划一个频道的中继路线。`current`：上一轮的路线（(from, to) → via），用于保持稳定。
pub fn plan(members: &[Member], current: &HashMap<(u32, u32), u32>) -> Vec<RelayRoute> {
    let by_id: HashMap<u32, &Member> = members.iter().map(|m| (m.id, m)).collect();

    // 每条需要中转的路线：候选中继按预计延迟从低到高
    struct Need {
        from: u32,
        to: u32,
        server_ms: u32,
        cands: Vec<(u32, u32)>, // (预计单程 ms, via)
    }
    let mut needs: Vec<Need> = Vec::new();
    for a in members {
        if a.muted {
            continue; // 闭麦的人不发语音
        }
        for b in members {
            if b.id == a.id || b.deafened || a.direct.contains(&b.id) || a.no_relay_to.contains(&b.id) {
                continue;
            }
            if a.server_only.contains(&b.id) || b.server_only.contains(&a.id) {
                continue; // 有一方手动选了走服务器（冲突时按更保守的）
            }
            let (Some(sa), Some(sb)) = (a.server_rtt, b.server_rtt) else { continue };
            let server_ms = sa + sb;
            // 手动指定了经谁中转（任一方指定都算，两个方向都经他）；两人指定的不一样 → 冲突，走服务器
            let manual = match (a.manual_via.get(&b.id), b.manual_via.get(&a.id)) {
                (Some(x), Some(y)) if x != y => continue,
                (Some(x), _) | (_, Some(x)) => Some(*x),
                _ => None,
            };
            let mut cands: Vec<(u32, u32)> = members
                .iter()
                .filter(|r| r.id != a.id && r.id != b.id && r.slots > 0 && a.direct.contains(&r.id))
                .filter(|r| manual.is_none_or(|x| r.id == x))
                .filter_map(|r| {
                    // 手动指定的：链路只要通着就用（不看丢包、抖动门槛）；自动的：两边都要稳定
                    let link = |to: u32| if manual.is_some() { r.links.get(&to).map(|l| l.0.max(1)) } else { r.good_link(to) };
                    let ms = link(a.id)? + link(b.id)? + FORWARD_COST_MS;
                    Some((ms, r.id))
                })
                // 手动指定的不要求比服务器快
                .filter(|(ms, _)| manual.is_some() || ms + MIN_GAIN_MS <= server_ms)
                .collect();
            if cands.is_empty() {
                continue;
            }
            cands.sort();
            needs.push(Need { from: a.id, to: b.id, server_ms, cands });
        }
    }
    // 收益大的先分配名额
    needs.sort_by_key(|n| (std::cmp::Reverse(n.server_ms - n.cands[0].0), n.from, n.to));

    let mut used: HashMap<u32, u16> = HashMap::new();
    let mut out = Vec::new();
    for n in needs {
        let free = |via: u32, used: &HashMap<u32, u16>| used.get(&via).copied().unwrap_or(0) < by_id[&via].slots;
        let best = n.cands[0].0;
        let keep = current
            .get(&(n.from, n.to))
            .and_then(|via| n.cands.iter().find(|(_, v)| v == via))
            .filter(|(ms, via)| *ms <= best + KEEP_MARGIN_MS && free(*via, &used))
            .copied();
        let pick = keep.or_else(|| n.cands.iter().find(|(_, via)| free(*via, &used)).copied());
        if let Some((ms, via)) = pick {
            *used.entry(via).or_default() += 1;
            out.push(RelayRoute { from: n.from, to: n.to, via, ms, server_ms: n.server_ms });
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(id: u32, server_rtt: u32, direct: &[u32], links: &[(u32, u32, u8, u32)], slots: u16) -> Member {
        Member {
            id,
            server_rtt: Some(server_rtt),
            direct: direct.iter().copied().collect(),
            links: links.iter().map(|&(i, r, l, j)| (i, (r, l, j))).collect(),
            slots,
            ..Default::default()
        }
    }

    #[test]
    fn manual_server_route_blocks_relay() {
        // A(1)、B(2) 打不通，R(3) 和两人都直连良好：本来会安排 A→B 经 R
        let ms = |srv_only: &[u32]| {
            let mut a = m(1, 150, &[3], &[], 0);
            a.server_only = srv_only.iter().copied().collect();
            vec![a, m(2, 150, &[3], &[], 0), m(3, 150, &[1, 2], &[(1, 20, 0, 1), (2, 20, 0, 1)], 4)]
        };
        assert!(plan(&ms(&[]), &HashMap::new()).iter().any(|r| r.from == 1 && r.to == 2));
        // A 手动选了和 B 走服务器：两个方向都不安排
        let routes = plan(&ms(&[2]), &HashMap::new());
        assert!(!routes.iter().any(|r| (r.from, r.to) == (1, 2) || (r.from, r.to) == (2, 1)), "{routes:?}");
    }

    #[test]
    fn picks_lowest_latency_relay() {
        // 1 和 2 打不通；3、4 和两人都直连，4 更快
        let ms = vec![
            m(1, 120, &[3, 4], &[(3, 40, 0, 2), (4, 20, 0, 2)], 8),
            m(2, 140, &[3, 4], &[(3, 40, 0, 2), (4, 24, 0, 2)], 8),
            m(3, 100, &[1, 2, 4], &[(1, 40, 0, 2), (2, 40, 0, 2), (4, 10, 0, 1)], 8),
            m(4, 100, &[1, 2, 3], &[(1, 20, 0, 2), (2, 24, 0, 2), (3, 10, 0, 1)], 8),
        ];
        let r = plan(&ms, &HashMap::new());
        assert_eq!(r.len(), 2);
        assert!(r.iter().all(|x| x.via == 4));
        // 经 4：20 + 24 + 4 = 48 ms；经服务器：120 + 140 = 260 ms
        assert_eq!(r[0], RelayRoute { from: 1, to: 2, via: 4, ms: 48, server_ms: 260 });
        assert_eq!(r[1], RelayRoute { from: 2, to: 1, via: 4, ms: 48, server_ms: 260 });
    }

    #[test]
    fn server_is_kept_when_relay_is_not_faster() {
        // 服务器中转 30+30 = 60 ms；经 3 是 40+40+4 = 84 ms → 不换
        let ms = vec![
            m(1, 30, &[3], &[(3, 40, 0, 2)], 8),
            m(2, 30, &[3], &[(3, 40, 0, 2)], 8),
            m(3, 30, &[1, 2], &[(1, 40, 0, 2), (2, 40, 0, 2)], 8),
        ];
        assert!(plan(&ms, &HashMap::new()).is_empty());
    }

    #[test]
    fn bad_or_missing_links_are_not_used() {
        let base = |loss: u8, jitter: u32, a_direct: &[u32], slots: u16| {
            vec![
                m(1, 200, a_direct, &[(3, 30, 0, 2)], 8),
                m(2, 200, &[3], &[(3, 30, 0, 2)], 8),
                m(3, 100, &[1, 2], &[(1, 30, loss, jitter), (2, 30, 0, 2)], slots),
            ]
        };
        assert!(!plan(&base(0, 2, &[3], 8), &HashMap::new()).is_empty(), "sanity: good case relays");
        assert!(plan(&base(5, 2, &[3], 8), &HashMap::new()).iter().all(|r| r.from != 1), "lossy R–A link");
        assert!(plan(&base(0, 50, &[3], 8), &HashMap::new()).iter().all(|r| r.from != 1), "jittery R–A link");
        assert!(plan(&base(0, 2, &[], 8), &HashMap::new()).iter().all(|r| r.from != 1), "A does not send to R");
        assert!(plan(&base(0, 2, &[3], 0), &HashMap::new()).is_empty(), "R has relaying turned off");
        // 已经直连的不用中转
        let mut ms = base(0, 2, &[3, 2], 8);
        ms[1].direct.insert(1);
        assert!(plan(&ms, &HashMap::new()).is_empty());
        // 没上报服务器延迟的（旧版客户端）不参与
        let mut ms = base(0, 2, &[3], 8);
        ms[1].server_rtt = None;
        assert!(plan(&ms, &HashMap::new()).is_empty());
    }

    #[test]
    fn muted_and_deafened_need_no_route() {
        let mut ms = vec![
            m(1, 200, &[3], &[(3, 30, 0, 2)], 8),
            m(2, 200, &[3], &[(3, 30, 0, 2)], 8),
            m(3, 100, &[1, 2], &[(1, 30, 0, 2), (2, 30, 0, 2)], 8),
        ];
        ms[0].muted = true; // 1 不说话 → 不需要 1→2
        ms[0].deafened = true; // 1 不听 → 不需要 2→1
        assert!(plan(&ms, &HashMap::new()).is_empty());
    }

    #[test]
    fn slots_are_respected_and_spread() {
        // 1、2 都要发给 5，3 和 4 都能中转但各只有 1 个名额
        let ms = vec![
            m(1, 200, &[3, 4], &[(3, 20, 0, 1), (4, 30, 0, 1)], 0),
            m(2, 200, &[3, 4], &[(3, 20, 0, 1), (4, 30, 0, 1)], 0),
            m(3, 100, &[1, 2, 4, 5], &[(1, 20, 0, 1), (2, 20, 0, 1), (4, 5, 0, 1), (5, 20, 0, 1)], 1),
            m(4, 100, &[1, 2, 3, 5], &[(1, 30, 0, 1), (2, 30, 0, 1), (3, 5, 0, 1), (5, 30, 0, 1)], 1),
            m(5, 200, &[3, 4], &[(3, 20, 0, 1), (4, 30, 0, 1)], 0),
        ];
        let r = plan(&ms, &HashMap::new());
        let mut per: HashMap<u32, u32> = HashMap::new();
        for x in &r {
            *per.entry(x.via).or_default() += 1;
        }
        assert!(per.values().all(|&n| n <= 1), "{r:?}");
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn manual_via_is_used_both_ways_or_falls_back_to_server() {
        // 1、2 打不通；3、4 都能中转，4 更快。1 手动指定经 3
        let base = || {
            let mut a = m(1, 200, &[3, 4], &[(3, 40, 0, 2), (4, 20, 0, 2)], 0);
            a.manual_via.insert(2, 3);
            vec![
                a,
                m(2, 200, &[3, 4], &[(3, 40, 0, 2), (4, 20, 0, 2)], 0),
                m(3, 100, &[1, 2], &[(1, 40, 6, 2), (2, 40, 0, 2)], 8), // 3 到 1 有点丢包：自动不会选，手动照用
                m(4, 100, &[1, 2], &[(1, 20, 0, 2), (2, 20, 0, 2)], 8),
            ]
        };
        let r = plan(&base(), &HashMap::new());
        assert_eq!(r.len(), 2, "{r:?}");
        assert!(r.iter().all(|x| x.via == 3), "both directions via the chosen member: {r:?}");
        // 两人指定的不一样：冲突，走服务器
        let mut ms = base();
        ms[1].manual_via.insert(1, 4);
        assert!(plan(&ms, &HashMap::new()).is_empty());
        // 指定的人关了中转：不改用别人，走服务器
        let mut ms = base();
        ms[2].slots = 0;
        assert!(plan(&ms, &HashMap::new()).is_empty());
        // 对方选了走服务器：更保守的为准
        let mut ms = base();
        ms[1].server_only.insert(1);
        assert!(plan(&ms, &HashMap::new()).is_empty());
    }

    #[test]
    fn no_relay_to_fallback_listeners() {
        let mut ms = vec![
            m(1, 200, &[3], &[(3, 30, 0, 2)], 8),
            m(2, 200, &[3], &[(3, 30, 0, 2)], 8),
            m(3, 100, &[1, 2], &[(1, 30, 0, 2), (2, 30, 0, 2)], 8),
        ];
        assert_eq!(plan(&ms, &HashMap::new()).len(), 2);
        ms[0].no_relay_to.insert(2); // 1 说话时 2 没听到 → 1→2 改经服务器
        let r = plan(&ms, &HashMap::new());
        assert!(r.iter().all(|x| !(x.from == 1 && x.to == 2)), "{r:?}");
        assert!(r.iter().any(|x| x.from == 2 && x.to == 1), "the other direction is unaffected");
    }

    #[test]
    fn existing_route_is_sticky() {
        let ms = vec![
            m(1, 200, &[3, 4], &[(3, 30, 0, 2), (4, 24, 0, 2)], 8),
            m(2, 200, &[3, 4], &[(3, 30, 0, 2), (4, 24, 0, 2)], 8),
            m(3, 100, &[1, 2, 4], &[(1, 30, 0, 2), (2, 30, 0, 2), (4, 5, 0, 1)], 8),
            m(4, 100, &[1, 2, 3], &[(1, 24, 0, 2), (2, 24, 0, 2), (3, 5, 0, 1)], 8),
        ];
        // 没有历史：选更快的 4（24+24+4 = 52 ms）
        assert!(plan(&ms, &HashMap::new()).iter().all(|r| r.via == 4));
        // 已经在走 3（30+30+4 = 64 ms），只慢 12 ms：保留，不来回切
        let cur: HashMap<(u32, u32), u32> = [((1, 2), 3), ((2, 1), 3)].into();
        assert!(plan(&ms, &cur).iter().all(|r| r.via == 3));
        // 3 的链路变差了：换成 4
        let mut worse = ms.clone();
        worse[2].links.insert(1, (30, 10, 2));
        assert!(plan(&worse, &cur).iter().all(|r| r.via == 4));
    }
}
