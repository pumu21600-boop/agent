//! 思考标签分割 —— 移植自 OpenClaw `packages/markdown-core/src/reasoning-tag-parser.ts`（v2026.8.2）。
//!
//! 上游把 14 种标签一律视为思考通道（<think>/<thinking>/<thought>/<reasoning>/<internal>/
//! <antthinking>/antml:* / mm:*）。本产品 P7.7 只认 <think>，免费反代吐 <thinking>、
//! <reasoning> 时会漏剥离进正文——这里对齐上游词表，增量流式语义保持一致：
//! 标签可跨分片断裂，用 hold 缓冲半截标签；未闭合时流尾按当前通道归位。

/// 上游 REASONING_TAG_NAMES（14 种，含开闭共用名）。
pub const REASONING_TAG_NAMES: [&str; 14] = [
    "think",
    "thinking",
    "thought",
    "reasoning",
    "internal",
    "antthinking",
    "antml:think",
    "antml:thinking",
    "antml:thought",
    "antml:reasoning",
    "mm:think",
    "mm:thinking",
    "mm:thought",
    "mm:reasoning",
];

/// 分割器状态：in_think=当前在某个思考标签内部。
#[derive(Default)]
pub struct TagSplit {
    in_think: bool,
    hold: String,
}

impl TagSplit {
    /// 喂入一段增量，返回 (思考文本, 正文文本)。
    pub fn feed(&mut self, text: &str) -> (String, String) {
        let mut think = String::new();
        let mut content = String::new();
        for ch in text.chars() {
            self.hold.push(ch);
            if self.hold == "<" {
                continue; // 起始符，等更多字符
            }
            if let Some((name, is_close)) = parse_tag(&self.hold) {
                let raw = std::mem::take(&mut self.hold);
                if is_close {
                    if REASONING_TAG_NAMES.contains(&name.as_str()) {
                        // 表内闭合标签：正常闭合则退出思考态；孤立时直接丢弃
                        // （上游解析器同样吃掉孤立 tag token，不漏进正文）
                        if self.in_think {
                            self.in_think = false;
                        }
                    } else {
                        // 非思考闭合标签（如 </b>）：原样进当前通道
                        self.push_channel(&raw, &mut think, &mut content);
                    }
                } else if REASONING_TAG_NAMES.contains(&name.as_str()) {
                    self.flush_before_tag(&mut think, &mut content);
                    self.in_think = true;
                } else {
                    // 非思考标签：原样进当前通道
                    self.push_channel(&raw, &mut think, &mut content);
                }
            } else if !could_become_tag(&self.hold) {
                // 不可能凑成任何思考标签了：整段 hold 落入当前通道
                self.flush_before_tag(&mut think, &mut content);
            }
            // 否则 hold 还是某个标签的前缀，留给下一片
        }
        (think, content)
    }

    fn flush_before_tag(&mut self, think: &mut String, content: &mut String) {
        let s = std::mem::take(&mut self.hold);
        self.push_channel(&s, think, content);
    }

    fn push_channel(&self, s: &str, think: &mut String, content: &mut String) {
        if s.is_empty() {
            return;
        }
        if self.in_think {
            think.push_str(s);
        } else {
            content.push_str(s);
        }
    }

    /// 流结束：hold 里剩下的半截标签不是标签，归入当前通道。
    pub fn finish(&mut self) -> (String, String) {
        let s = std::mem::take(&mut self.hold);
        if s.is_empty() {
            return (String::new(), String::new());
        }
        if self.in_think {
            (s, String::new())
        } else {
            (String::new(), s)
        }
    }
}

/// 解析 hold 是否为完整标签，返回 (标签名, 是否闭合)。
fn parse_tag(hold: &str) -> Option<(String, bool)> {
    if !hold.starts_with('<') || !hold.ends_with('>') {
        return None;
    }
    let inner = &hold[1..hold.len() - 1];
    let (name, is_close) = if let Some(n) = inner.strip_prefix('/') {
        (n, true)
    } else {
        (inner, false)
    };
    let name = name.trim().to_lowercase();
    if name.is_empty() || name.contains(['<', '>', '/']) {
        return None;
    }
    Some((name, is_close))
}

/// hold 是否仍是「某个思考标签的合法前缀」（开/闭形态都算）。
fn could_become_tag(hold: &str) -> bool {
    if hold.is_empty() {
        return true;
    }
    if !hold.starts_with('<') {
        return false;
    }
    let inner = &hold[1..];
    let probe = inner.strip_prefix('/').unwrap_or(inner);
    if probe.contains('>') || probe.contains('<') {
        return false; // 已有 '>' 却没被 parse_tag 认可 → 非标签
    }
    let probe = probe.to_lowercase();
    // 完整标签名或其前缀
    REASONING_TAG_NAMES
        .iter()
        .any(|n| n.starts_with(probe.as_str()) || probe.starts_with(*n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openclaw_tag_table() {
        // 上游词表核心形态：无标签原样过
        let mut ts = TagSplit::default();
        assert_eq!(ts.feed("普通回答"), ("".into(), "普通回答".into()));
        // 每种标签单独跑一遍：思考进思考通道
        for name in REASONING_TAG_NAMES {
            let open = format!("<{name}>先想想</{name}>答案");
            let mut ts = TagSplit::default();
            let (t, c) = ts.feed(&open);
            assert_eq!(t, "先想想", "标签 <{name}> 应剥离");
            assert_eq!(c, "答案");
        }
    }

    #[test]
    fn cross_chunk_and_edge_cases() {
        // 跨分片断裂："<thi|nk>AB</thi|nk>CD"
        let mut ts = TagSplit::default();
        assert_eq!(ts.feed("<thi"), ("".into(), "".into()));
        assert_eq!(ts.feed("nk>AB</thi"), ("AB".into(), "".into()));
        assert_eq!(ts.feed("nk>CD"), ("".into(), "CD".into()));
        assert_eq!(ts.finish(), ("".into(), "".into()));
        // 未闭合 <think>（流在思考中被截断）：全部走思考
        let mut ts = TagSplit::default();
        assert_eq!(ts.feed("<think>还没想完"), ("还没想完".into(), "".into()));
        // 正文里出现 "<thinking" 伪前缀但不是标签：'a' 立即归正文，"<thinking" 留 hold 到流尾
        let mut ts = TagSplit::default();
        let (t, c) = ts.feed("a<thinking");
        assert_eq!((t.as_str(), c.as_str()), ("", "a"));
        assert_eq!(ts.finish(), ("".into(), "<thinking".into()));
        // 孤立 </think>（不在思考态）：当普通文本
        let mut ts = TagSplit::default();
        assert_eq!(ts.feed("x</think>y"), ("".into(), "xy".into()));
        // 非思考标签：原样保留在正文
        let mut ts = TagSplit::default();
        assert_eq!(ts.feed("<b>加粗</b>"), ("".into(), "<b>加粗</b>".into()));
        // 截断在半截标签：finish 归位正文
        let mut ts = TagSplit::default();
        assert_eq!(ts.feed("尾</thi"), ("".into(), "尾".into()));
        assert_eq!(ts.finish(), ("".into(), "</thi".into()));
    }
}
