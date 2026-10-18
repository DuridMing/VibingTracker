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

impl Item<'_> {
    fn ts(&self) -> u64 {
        match self {
            Item::Event(e) => e.ts_seen,
            Item::Ack(a) => a.at,
        }
    }
}

fn apply(live: &mut Live, item: &Item) {
    match item {
        Item::Event(e) => live.apply(e),
        Item::Ack(a) => {
            live.ack(&a.session, &a.kind, &a.reference);
        }
    }
}

/// `at` 時的畫面（`with_snapshot`）與 (`at`, `until`] 之間的 frame，每個 frame 帶 `ts`。
/// 同一個時間點的 Event 一起放進同一個 frame；放了 `limit` 個 Event 後在時間點邊界停下，
/// `next` 是最後放進來的時間點（下一段用 `at=next` 接著要），範圍內都放完時是 null。
pub fn replay(
    events: &[Event],
    acks: &[Ack],
    at: u64,
    until: u64,
    limit: usize,
    offset_min: i32,
    with_snapshot: bool,
) -> Value {
    let mut items: Vec<Item> = events
        .iter()
        .map(Item::Event)
        .chain(acks.iter().map(Item::Ack))
        .collect();
    // 同一毫秒：Event 依 (session, seq)；確認排在 Event 之後（確認的是已經出現的列）
    items.sort_by(|a, b| {
        let key = |i: &Item| match i {
            Item::Event(e) => (i.ts(), 0, e.session.clone(), e.seq),
            Item::Ack(a) => (i.ts(), 1, a.session.clone(), 0),
        };
        key(a).cmp(&key(b))
    });

    let mut live = Live::new();
    live.register_offset(offset_min);
    let mut i = 0;
    while i < items.len() && items[i].ts() <= at {
        apply(&mut live, &items[i]);
        i += 1;
    }
    // 把到 at 為止的變化收掉（也讓結束超過 2 小時的 session 依 at 移出），frame 從 at 之後算
    let _ = live.flush(at);
    let snapshot = with_snapshot.then(|| live.snapshot(&Channel::All, offset_min, at));

    let mut frames = Vec::new();
    let mut placed = 0;
    let mut next = None;
    while i < items.len() && items[i].ts() <= until {
        if placed >= limit {
            next = frames.last().and_then(|f: &Value| f["ts"].as_u64());
            break;
        }
        let ts = items[i].ts();
        while i < items.len() && items[i].ts() == ts {
            if matches!(items[i], Item::Event(_)) {
                placed += 1;
            }
            apply(&mut live, &items[i]);
            i += 1;
        }
        if let Some(mut f) = live
            .flush(ts)
            .and_then(|f| f.to_json(&Channel::All, offset_min))
        {
            f["ts"] = json!(ts);
            frames.push(f);
        }
    }
    let mut out = json!({ "at": at, "until": until, "frames": frames, "next": next });
    if let Some(s) = snapshot {
        out["snapshot"] = s;
    }
    out
}
