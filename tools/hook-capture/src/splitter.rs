/// 把分次讀到的位元組切成完整的行；尾端不完整的部分留到下次。
#[derive(Default)]
pub struct LineSplitter {
    pending: Vec<u8>,
}

impl LineSplitter {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        self.pending.extend_from_slice(bytes);
        let mut lines = Vec::new();
        let mut start = 0;
        for (i, b) in self.pending.iter().enumerate() {
            if *b == b'\n' {
                lines.push(self.pending[start..i].to_vec());
                start = i + 1;
            }
        }
        self.pending.drain(..start);
        lines
    }

    /// 還沒等到 `\n` 的位元組數。
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}
