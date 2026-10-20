//! 全專案時間軸回放（#729）：把記錄下來的 Event 重新套進一份獨立的 [`Live`]，算出任意時刻 T 的主頁畫面
//! （形狀同 `*` 的 snapshot）與 T 之後逐時間點的 frame。純邏輯、不碰 I/O，時間只來自 Event 的 `ts_seen`
//! 與確認時間；session 狀態與注意佇列一樣只來自 collector 記錄的 Event（不變式 8），前端照即時的方式套用。

use serde_json::{Value, json};

use crate::event::Event;
use crate::live::{Channel, Live};

/// 注意佇列被「已確認」的時間（store 的 `attention.acked_at`）：不是 Event，回放時另外套。
#[derive(Clone, Debug)]
pub struct Ack {
    pub session: String,
    pub kind: String,
    pub reference: String,
    pub at: u64,
}

enum Item<'a> {
    Event(&'a Event),
    Ack(&'a Ack),
}

fn apply(live: &mut Live, item: &Item) {
    match item {
        Item::Event(e) => live.apply(e),
        Item::Ack(a) => {
            live.ack(&a.session, &a.kind, &a.reference);
        }
    }
}

/// 一段回放的範圍。`cut`：store 只讀到這個時間之前（[`crate::store::Store::timeline`] 的本頁截止，E-L1）；
/// 這一段只放比它早的時間點，`next` 接著從最後放進來的時間點要。
pub struct Page {
    pub at: u64,
    pub until: u64,
    pub cut: Option<u64>,
    pub limit: usize,
    pub offset_min: i32,
    pub with_snapshot: bool,
}

/// [`replay_page`]，資料沒有被截（測試與小範圍用）。
pub fn replay(
    events: &[Event],
    acks: &[Ack],
    at: u64,
    until: u64,
    limit: usize,
    offset_min: i32,
    with_snapshot: bool,
) -> Value {
    replay_page(
        events,
        acks,
        &Page {
            at,
            until,
            cut: None,
            limit,
            offset_min,
            with_snapshot,
        },
    )
}

/// 依套用順序排好的項目與各自的「時間點」：跨 session 依 ts_seen 合併，同 session 內一律依 seq（不變式 1，C9），
/// 確認排在同一毫秒的 Event 之後。ts_seen 偶爾倒退時，時間點取到目前為止的最大值，frame 的時間只往前走。
/// 各 session 已依 seq 排好時是線性合併，排序鍵借用不拷貝（E-L7）。
/// 合併用的堆頂：(ts, Event 先於確認, session, seq, 來源)。
type Head<'a> = std::cmp::Reverse<(u64, u8, &'a str, u64, usize)>;

fn ordered<'a>(events: &'a [Event], acks: &'a [Ack]) -> Vec<(u64, Item<'a>)> {
    use std::cmp::Reverse;
    use std::collections::{BTreeMap, BinaryHeap};
    let mut by_session: BTreeMap<&str, Vec<&Event>> = BTreeMap::new();
    for e in events {
        by_session.entry(&e.session).or_default().push(e);
    }
    let mut lists: Vec<Vec<&Event>> = by_session.into_values().collect();
    for l in &mut lists {
        if !l.is_sorted_by_key(|e| e.seq) {
            l.sort_by_key(|e| e.seq);
        }
    }
    let mut acks: Vec<&Ack> = acks.iter().collect();
    acks.sort_by_key(|a| (a.at, a.session.as_str()));
    // 堆裡放每個來源的開頭：(ts, Event 先於確認, session, seq, 來源)；確認是最後一個來源
    let ack_src = lists.len();
    let mut heads = vec![0usize; lists.len() + 1];
    let key = |src: usize, i: usize| -> Option<Head<'a>> {
        if src == ack_src {
            acks.get(i)
                .map(|a| Reverse((a.at, 1, a.session.as_str(), 0, src)))
        } else {
            lists[src]
                .get(i)
                .map(|e| Reverse((e.ts_seen, 0, e.session.as_str(), e.seq, src)))
        }
    };
    let mut heap: BinaryHeap<_> = (0..=ack_src).filter_map(|src| key(src, 0)).collect();
    let mut out = Vec::with_capacity(events.len() + acks.len());
    let mut clock = 0;
    while let Some(Reverse((ts, _, _, _, src))) = heap.pop() {
        let i = heads[src];
        heads[src] += 1;
        clock = clock.max(ts);
        let item = if src == ack_src {
            Item::Ack(acks[i])
        } else {
            Item::Event(lists[src][i])
        };
        out.push((clock, item));
        if let Some(k) = key(src, i + 1) {
            heap.push(k);
        }
    }
    out
}

/// 追到 `at` 時每套多少個項目丟一次還沒送出的變更：那些 frame 沒有人要，累積起來就是整段 Event 的一份拷貝（C16）。
const CATCH_UP_DISCARD: usize = 1024;

/// `at` 時的畫面（`with_snapshot`）與 (`at`, `until`] 之間的 frame，每個 frame 帶 `ts`。
/// 同一個時間點的 Event 一起放進同一個 frame；放了 `limit` 個 Event 後在時間點邊界停下，
/// `next` 是最後放進來的時間點（下一段用 `at=next` 接著要），範圍內都放完時是 null。
pub fn replay_page(events: &[Event], acks: &[Ack], page: &Page) -> Value {
    let Page {
        at,
        until,
        cut,
        limit,
        offset_min,
        with_snapshot,
    } = *page;
    let items = ordered(events, acks);
    // 回放的 Live 不用「載入更多」也沒有人斷線接續（E-L8）
    let mut live = Live::with_limits(crate::live::ALL_STREAM_PER_SESSION, 0);
    live.register_offset(offset_min);
    let mut i = 0;
    while i < items.len() && items[i].0 <= at {
        apply(&mut live, &items[i].1);
        i += 1;
        if i % CATCH_UP_DISCARD == 0 {
            live.discard_pending();
        }
    }
    // 到 at 為止的變化不送（snapshot 已經是那時的畫面）；flush 讓結束超過 2 小時的 session 依 at 移出、記下 KPI 的基準
    live.discard_pending();
    let _ = live.flush(at);
    let snapshot = with_snapshot.then(|| live.snapshot(&Channel::All, offset_min, at));

    let end = cut.map_or(until, |c| c.saturating_sub(1).min(until));
    let mut frames = Vec::new();
    let mut placed = 0;
    let mut last = None;
    let mut next = None;
    while i < items.len() && items[i].0 <= end {
        if placed >= limit {
            next = last;
            break;
        }
        let ts = items[i].0;
        while i < items.len() && items[i].0 == ts {
            if matches!(items[i].1, Item::Event(_)) {
                placed += 1;
            }
            apply(&mut live, &items[i].1);
            i += 1;
        }
        last = Some(ts);
        if let Some(mut f) = live
            .flush(ts)
            .and_then(|f| f.to_json(&Channel::All, offset_min))
        {
            f["ts"] = json!(ts);
            frames.push(f);
        }
    }
    // store 只讀到 cut 之前：放完了也還沒到 until，從最後放進來的時間點接著要
    if next.is_none() && cut.is_some_and(|c| c <= until) {
        next = last.or(Some(end));
    }
    let mut out = json!({ "at": at, "until": until, "frames": frames, "next": next });
    if let Some(s) = snapshot {
        out["snapshot"] = s;
    }
    out
}
