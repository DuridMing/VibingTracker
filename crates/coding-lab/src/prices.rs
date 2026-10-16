//! 模型單價（#633）：USD／每百萬 tokens。
//!
//! 內建表來自 Anthropic 第一方 API 價目（2026-10-06）；cache 寫入固定是輸入價的 1.25 倍（5 分鐘 TTL）
//! 與 2 倍（1 小時 TTL）。使用者可用 `~/.coding-lab/prices.json` 覆寫（#643）。不認得的模型費用記 0，不猜。

use std::collections::HashMap;

use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
}

pub struct Prices {
    table: HashMap<String, Price>,
}

const BUILTIN: &[(&str, f64, f64, f64)] = &[
    // (模型, 輸入, 輸出, cache 讀)
    ("claude-fable-5-1", 10.0, 50.0, 0.25),
    ("claude-mythos-5-1", 10.0, 50.0, 0.25),
    ("claude-fable-5", 10.0, 50.0, 1.0),
    ("claude-opus-5-5", 4.0, 20.0, 0.20),
    ("claude-opus-5", 5.0, 25.0, 0.50),
    ("claude-opus-4-8", 5.0, 25.0, 0.50),
    ("claude-opus-4-7", 5.0, 25.0, 0.50),
    ("claude-opus-4-6", 5.0, 25.0, 0.50),
    ("claude-sonnet-5-5", 2.0, 10.0, 0.20),
    ("claude-sonnet-5", 2.0, 10.0, 0.20),
    ("claude-sonnet-4-6", 3.0, 15.0, 0.30),
    // Haiku 5.5：100K tokens 以內的價格（超過是 $0.50／$2.50，v1 不分段）
    ("claude-haiku-5-5", 0.10, 0.50, 0.01),
    ("claude-haiku-4-5", 1.0, 5.0, 0.10),
];

impl Prices {
    pub fn builtin() -> Prices {
        let table = BUILTIN
            .iter()
            .map(|(m, i, o, r)| {
                (
                    m.to_string(),
                    Price {
                        input: *i,
                        output: *o,
                        cache_read: *r,
                    },
                )
            })
            .collect();
        Prices { table }
    }

    pub fn get(&self, model: &str) -> Option<Price> {
        // transcript 的模型名稱可能帶 `[1m]` 之類的後綴
        let base = model.split('[').next().unwrap_or(model);
        self.table.get(base).copied()
    }

    /// transcript `message.usage` 的費用（USD）。
    pub fn cost(&self, model: &str, usage: &Value) -> f64 {
        let Some(p) = self.get(model) else { return 0.0 };
        let n = |v: &Value| v.as_u64().unwrap_or(0) as f64;
        let cc = &usage["cache_creation"];
        let (w5, w1) = if cc.is_object() {
            (
                n(&cc["ephemeral_5m_input_tokens"]),
                n(&cc["ephemeral_1h_input_tokens"]),
            )
        } else {
            (n(&usage["cache_creation_input_tokens"]), 0.0)
        };
        (n(&usage["input_tokens"]) * p.input
            + n(&usage["output_tokens"]) * p.output
            + n(&usage["cache_read_input_tokens"]) * p.cache_read
            + w5 * p.input * 1.25
            + w1 * p.input * 2.0)
            / 1_000_000.0
    }
}
