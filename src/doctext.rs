//! Office 文档文本抽取（预览面板「能看内容」用，best-effort）。
//!
//! docx/pptx/xlsx 是 zip 容器，条目为 deflate/stdio 压缩的 XML；ODF（odt/ods/odp）
//! 同理但正文在 content.xml。zip 中央目录手写解析（PK 规范固定字段，~80 行），
//! 解压只补 flate2（纯 Rust miniz_oxide 后端），不引完整 zip 依赖。
//! XML 只做「去标签 + 段落换行 + 实体解码」——要的是可读文本，不是保真排版。

use serde_json::json;
use std::io::Read;
use std::path::Path;

/// 预览文本上限：头部 100k + 尾部 60k，中间用文件预览同一颗省略标记
/// （前端 previewTruncated 认这颗字符串，命中即不给编辑）。
const HEAD: usize = 100_000;
const TAIL: usize = 60_000;
const OMITTED: &str = "\n……[中间省略]……\n";

/// 按扩展名抽取文本。不认识的类型返回 Err（调用方给人话错误）。
pub fn extract(path: &Path) -> anyhow::Result<String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let bytes = std::fs::read(path)?;
    match ext.as_str() {
        "docx" => xml_text_from(&bytes, &["word/document.xml"]),
        "xlsx" => xml_text_from(&bytes, &["xl/sharedStrings.xml"]),
        "pptx" => pptx_slides(&bytes),
        "odt" | "ods" | "odp" => xml_text_from(&bytes, &["content.xml"]),
        other => anyhow::bail!("不支持的文档类型: .{other}"),
    }
}

/// pptx 结构化（预览面板分页渲染用）：[{n, text}]，n=幻灯片页码。
pub fn pptx_slides_json(path: &Path) -> anyhow::Result<String> {
    let bytes = std::fs::read(path)?;
    let mut names = zip_list(&bytes)?
        .into_iter()
        .filter(|n| {
            n.starts_with("ppt/slides/slide") && n.ends_with(".xml") && !n.contains("Layout") && !n.contains("Master")
        })
        .collect::<Vec<_>>();
    names.sort_by_key(|n| {
        n.trim_start_matches("ppt/slides/slide")
            .trim_end_matches(".xml")
            .parse::<u32>()
            .unwrap_or(u32::MAX)
    });
    let mut out = Vec::new();
    for name in &names {
        if let Some(data) = zip_read(&bytes, name)? {
            let n = name
                .trim_start_matches("ppt/slides/slide")
                .trim_end_matches(".xml")
                .parse::<u32>()
                .unwrap_or(0);
            out.push(json!({ "n": n, "text": xml_to_text(&data) }));
        }
    }
    if out.is_empty() {
        anyhow::bail!("文档里没有幻灯片（可能是空演示文稿或加密文档）");
    }
    Ok(serde_json::to_string(&out).unwrap_or_else(|_| "[]".into()))
}

/// 依次抽取容器内列出的 XML 条目并转文本（条目缺失跳过，全缺报错）。
fn xml_text_from(bytes: &[u8], names: &[&str]) -> anyhow::Result<String> {
    let mut out = String::new();
    for name in names {
        if let Some(data) = zip_read(bytes, name)? {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&xml_to_text(&data));
        }
    }
    if out.trim().is_empty() {
        anyhow::bail!("文档里没有可提取的文本（可能是空文档或加密文档）");
    }
    Ok(truncate(out))
}

fn pptx_slides(bytes: &[u8]) -> anyhow::Result<String> {
    let names = zip_list(bytes)?
        .into_iter()
        .filter(|n| {
            n.starts_with("ppt/slides/slide") && n.ends_with(".xml") && !n.contains("Layout")
                && !n.contains("Master")
        })
        .collect::<Vec<_>>();
    if names.is_empty() {
        anyhow::bail!("文档里没有幻灯片（可能是空演示文稿或加密文档）");
    }
    let mut sorted = names;
    sorted.sort_by_key(|n| {
        n.trim_start_matches("ppt/slides/slide")
            .trim_end_matches(".xml")
            .parse::<u32>()
            .unwrap_or(u32::MAX)
    });
    let mut out = String::new();
    for (i, name) in sorted.iter().enumerate() {
        let Ok(Some(data)) = zip_read(bytes, name) else { continue };
        let idx = name
            .trim_start_matches("ppt/slides/slide")
            .trim_end_matches(".xml")
            .parse::<u32>()
            .unwrap_or(i as u32 + 1);
        if i > 0 {
            out.push_str("\n\n");
        }
        out.push_str(&format!("—— 幻灯片 {idx} ——\n{}", xml_to_text(&data)));
    }
    if out.trim().is_empty() {
        anyhow::bail!("幻灯片里没有可提取的文本");
    }
    Ok(truncate(out))
}

// —— zip 读取（EOCD → 中央目录 → 本地头定位数据段）——

fn u16le(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn u32le(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// 解析中央目录，返回 (名字, 压缩方法, 数据段起始偏移, 压缩后大小)。
fn zip_entries(bytes: &[u8]) -> anyhow::Result<Vec<(String, u16, usize, usize)>> {
    // EOCD 至少 22 字节，注释区最长 65535：从尾部往回扫签名
    if bytes.len() < 22 {
        anyhow::bail!("不是 zip 容器（太小）");
    }
    let mut eocd = None;
    let floor = bytes.len().saturating_sub(22 + 65_535);
    let mut i = bytes.len() - 22;
    loop {
        if u32le(bytes, i) == 0x0605_4b50 {
            eocd = Some(i);
            break;
        }
        if i == floor {
            break;
        }
        i -= 1;
    }
    let Some(eocd) = eocd else { anyhow::bail!("不是 zip 容器（找不到中央目录尾）") };
    let count = u16le(bytes, eocd + 10) as usize;
    let mut off = u32le(bytes, eocd + 16) as usize;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        if off + 46 > bytes.len() || u32le(bytes, off) != 0x0201_4b50 {
            anyhow::bail!("zip 中央目录损坏");
        }
        let method = u16le(bytes, off + 10);
        let csize = u32le(bytes, off + 20) as usize;
        let nlen = u16le(bytes, off + 28) as usize;
        let elen = u16le(bytes, off + 30) as usize;
        let clen = u16le(bytes, off + 32) as usize;
        let lfh = u32le(bytes, off + 42) as usize;
        let name = String::from_utf8_lossy(&bytes[off + 46..off + 46 + nlen]).into_owned();
        // 本地文件头的名字/扩展字段长度可能与中央目录不同，按本地头自己的字段算
        if lfh + 30 <= bytes.len() && u32le(bytes, lfh) == 0x0403_4b50 {
            let data_at = lfh + 30 + u16le(bytes, lfh + 26) as usize + u16le(bytes, lfh + 28) as usize;
            out.push((name, method, data_at, csize));
        }
        off += 46 + nlen + elen + clen;
    }
    Ok(out)
}

fn zip_list(bytes: &[u8]) -> anyhow::Result<Vec<String>> {
    Ok(zip_entries(bytes)?.into_iter().map(|(n, _, _, _)| n).collect())
}

/// 按名字取一个条目并解压（method 0 原样 / 8 deflate；其余不支持）。
fn zip_read(bytes: &[u8], want: &str) -> anyhow::Result<Option<Vec<u8>>> {
    for (name, method, at, csize) in zip_entries(bytes)? {
        if name != want {
            continue;
        }
        if at + csize > bytes.len() {
            anyhow::bail!("zip 条目数据越界: {name}");
        }
        let raw = &bytes[at..at + csize];
        let data = match method {
            0 => raw.to_vec(),
            8 => {
                let mut out = Vec::new();
                flate2::read::DeflateDecoder::new(raw).read_to_end(&mut out)?;
                out
            }
            m => anyhow::bail!("不支持的压缩方法 {m}: {name}"),
        };
        return Ok(Some(data));
    }
    Ok(None)
}

// —— XML → 文本 ——

/// 去标签：段落/换行标签翻成换行，tab 翻成制表，其余标签丢弃、标签外文本保留。
fn xml_to_text(xml: &[u8]) -> String {
    let s = String::from_utf8_lossy(xml);
    let mut out = String::with_capacity(s.len() / 2);
    let mut rest = s.as_ref();
    while let Some(lt) = rest.find('<') {
        out.push_str(&decode_entities(&rest[..lt]));
        let Some(gt) = rest[lt..].find('>') else { break };
        let tag = &rest[lt + 1..lt + gt];
        let t = tag.trim();
        if t.starts_with("/w:p") || t.starts_with("/a:p") || t.starts_with("/text:p") || t.starts_with("/text:h") {
            out.push('\n');
        } else if t.starts_with("w:tab") || t.starts_with("a:tab") {
            out.push('\t');
        } else if t.starts_with("w:br") || t.starts_with("a:br") {
            out.push('\n');
        }
        rest = &rest[lt + gt + 1..];
    }
    // 段落间的连续空行压成单个换行，别给预览灌十行空白
    let mut compact = String::with_capacity(out.len());
    let mut prev_nl = false;
    for ch in out.chars() {
        let nl = ch == '\n';
        if !(nl && prev_nl) {
            compact.push(ch);
        }
        prev_nl = nl;
    }
    compact.trim().to_string()
}

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        let end = tail.find(';').unwrap_or(0);
        if end > 1 && end <= 10 {
            let ent = &tail[1..end];
            let ch = match ent {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ if ent.starts_with("#x") || ent.starts_with("#X") => {
                    u32::from_str_radix(&ent[2..], 16).ok().and_then(char::from_u32)
                }
                _ if ent.starts_with('#') => ent[1..].parse::<u32>().ok().and_then(char::from_u32),
                _ => None,
            };
            match ch {
                Some(c) => {
                    out.push(c);
                    rest = &tail[end + 1..];
                }
                None => {
                    out.push('&');
                    rest = &tail[1..];
                }
            }
        } else {
            out.push('&');
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    out
}

fn truncate(s: String) -> String {
    let n = s.chars().count();
    if n <= HEAD + TAIL {
        return s;
    }
    let head: String = s.chars().take(HEAD).collect();
    let tail: String = s.chars().skip(n - TAIL).collect();
    format!("{head}{OMITTED}{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手写一个最小 zip（条目按 method 存：0=原样 8=deflate），给解析器喂已知结构。
    fn make_zip_m(method: u16, entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut body = Vec::new();
        let mut central = Vec::new();
        for (name, data) in entries {
            let lfh_at = body.len();
            let nb = name.as_bytes();
            body.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            body.extend_from_slice(&[20, 0, 0, 0]); // 版本/flag
            body.extend_from_slice(&method.to_le_bytes()); // 0=stored 8=deflate
            body.extend_from_slice(&[0, 0, 0, 0]); // 时间/日期
            body.extend_from_slice(&0u32.to_le_bytes()); // crc32（解析器不校验，占位）
            body.extend_from_slice(&(data.len() as u32).to_le_bytes());
            body.extend_from_slice(&(data.len() as u32).to_le_bytes());
            body.extend_from_slice(&(nb.len() as u16).to_le_bytes());
            body.extend_from_slice(&0u16.to_le_bytes());
            body.extend_from_slice(nb);
            body.extend_from_slice(data);

            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            central.extend_from_slice(&[20, 0, 20, 0, 0, 0]);
            central.extend_from_slice(&method.to_le_bytes());
            central.extend_from_slice(&[0, 0, 0, 0]);
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(nb.len() as u16).to_le_bytes());
            central.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]); // 扩展/注释/盘号/内部外部属性
            central.extend_from_slice(&(lfh_at as u32).to_le_bytes());
            central.extend_from_slice(nb);
        }
        let cd_at = body.len() as u32;
        let mut zip = body;
        zip.extend_from_slice(&central);
        zip.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        zip.extend_from_slice(&[0, 0, 0, 0]); // 盘号们
        zip.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        zip.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        zip.extend_from_slice(&(central.len() as u32).to_le_bytes());
        zip.extend_from_slice(&cd_at.to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes()); // 注释长
        zip
    }

    fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        make_zip_m(0, entries)
    }

    #[test]
    fn docx_paragraphs_become_lines() {
        // 字节串字面量只能 ASCII，带中文的 XML 用 str.as_bytes()
        let xml = r#"<?xml version="1.0"?><w:document><w:body><w:p><w:r><w:t>Hello &amp; &lt;hi&gt;</w:t></w:r></w:p><w:p><w:r><w:t>世界</w:t><w:br/><w:t>下一行</w:t></w:r></w:p></w:body></w:document>"#;
        let zip = make_zip(&[("word/document.xml", xml.as_bytes())]);
        let text = extract_text_for("dx", &zip, "docx");
        assert!(text.contains("Hello & <hi>"), "实体解码: {text}");
        assert!(text.contains("Hello & <hi>\n世界\n下一行"), "段落换行: {text}");
    }

    #[test]
    fn pptx_slides_numbered_in_order() {
        let xml1 = r#"<p:sld><p:txBody><a:p><a:t>第一页</a:t></a:p></p:txBody></p:sld>"#;
        let xml2 = r#"<p:sld><p:txBody><a:p><a:t>第二页</a:t></a:p></p:txBody></p:sld>"#;
        let zip = make_zip(&[("ppt/slides/slide2.xml", xml2.as_bytes()), ("ppt/slides/slide1.xml", xml1.as_bytes())]);
        let text = extract_text_for("pp", &zip, "pptx");
        assert!(text.contains("幻灯片 1 ——\n第一页"), "顺序按数字: {text}");
        assert!(text.find("第一页").unwrap() < text.find("第二页").unwrap());
    }

    #[test]
    fn deflate_entry_is_inflated() {
        // 用 flate2 压一段再塞进 zip，验证 method 8 路径
        use std::io::Write as _;
        let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all("<w:p><w:t>压缩过的正文</w:t></w:p>".as_bytes()).unwrap();
        let data = enc.finish().unwrap();
        let zip = make_zip_m(8, &[("word/document.xml", &data)]);
        let text = extract_text_for("df", &zip, "docx");
        assert_eq!(text, "压缩过的正文");
    }

    #[test]
    fn oversize_text_gets_head_tail_with_marker() {
        let long = format!("<w:p><w:t>{}</w:t></w:p>", "字".repeat(HEAD + TAIL + 5_000));
        let zip = make_zip(&[("word/document.xml", long.as_bytes())]);
        let text = extract_text_for("big", &zip, "docx");
        assert!(text.contains("……[中间省略"), "超长文本应头尾压缩: {}", text.len());
    }

    /// 测试直用字节容器（extract() 收文件路径）。tag 隔离并行测试的临时目录
    /// （同 pid 下 remove_dir_all 会冲掉别的测试正在用的文件）。
    fn extract_text_for(tag: &str, zip: &[u8], ext: &str) -> String {
        let dir = std::env::temp_dir().join(format!("agent_doctext_{}_{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(format!("t.{ext}"));
        std::fs::write(&p, zip).unwrap();
        let out = extract(&p).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        out
    }
}
