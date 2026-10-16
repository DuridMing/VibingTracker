//! 模型單價（#633）：USD／每百萬 tokens。
//!
//! 內建表來自 Anthropic 第一方 API 價目（2026-10-06）；cache 寫入固定是輸入價的 1.25 倍（5 分鐘 TTL）
//! 與 2 倍（1 小時 TTL）。使用者可用 `~/.coding-lab/prices.json` 覆寫（#678，見 [`Prices::load`]）。
//! 不認得的模型費用記 0，不猜。

use std::collections::HashMap;
use std::path::Path;

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

    /// `<home>/prices.json` 覆寫（#678）：`{ "<模型>": { "input", "output", "cacheRead" } }`，USD／每百萬 tokens。
    /// 有列的模型蓋過內建、沒列的照用內建、可新增；`_` 開頭的鍵是註解。檔案不存在不算錯；
    /// 壞掉或數字不合理就整個退回內建表，第二個值是要寫進 log 的警告（不讓 collector 起不來，不變式 4）。
    pub fn load(path: &Path) -> (Prices, Option<String>) {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Prices::builtin(), None),
            Err(e) => {
                return (
                    Prices::builtin(),
                    Some(format!("讀不到 {}：{e}，改用內建單價", path.display())),
                );
            }
        };
        match parse_overrides(&text) {
            Ok(overrides) => {
                let mut prices = Prices::builtin();
                prices.table.extend(overrides);
                (prices, None)
            }
            Err(e) => (
                Prices::builtin(),
                Some(format!("{} 有錯（{e}），改用內建單價", path.display())),
            ),
        }
    }

    pub fn get(&self, model: &str) -> Option<Price> {
        self.table.get(base_model(model)).copied()
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

/// transcript 的模型名稱可能帶 `[1m]` 之類的後綴；比對時去掉（prices.json 的鍵也一樣）。
fn base_model(model: &str) -> &str {
    model.split('[').next().unwrap_or(model)
}

fn parse_overrides(text: &str) -> Result<Vec<(String, Price)>, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let models = v.as_object().ok_or("最外層要是物件")?;
    let mut out = Vec::new();
    for (model, row) in models {
        if model.starts_with('_') {
            continue;
        }
        let field = |name: &str| {
            row.get(name)
                .and_then(Value::as_f64)
                .filter(|n| n.is_finite() && *n >= 0.0)
                .ok_or_else(|| format!("{model}.{name} 要是不小於 0 的數字"))
        };
        let price = Price {
            input: field("input")?,
            output: field("output")?,
            cache_read: field("cacheRead")?,
        };
        out.push((base_model(model).to_string(), price));
    }
    Ok(out)
}
