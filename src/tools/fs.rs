//! 文件类工具:read_file / read_page / write_file / edit_file(替换引擎 apply_edit + 512KB 体积上限)。

use super::*;

/// read_file/read_page 共用的「读不动」人话化：二进制/图片文件别扔 os error 天书，
/// 告诉模型这东西不是文本、该走什么路（图片给用户发过多模态消息，别拿文本工具啃）。
fn read_text_err(full: &std::path::Path, e: std::io::Error) -> String {
    let ext = full
        .extension()
        .map(|x| x.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let media = ["jpg", "jpeg", "png", "gif", "webp", "bmp", "ico", "pdf", "docx", "xlsx", "pptx", "zip", "mp3", "mp4", "wav", "exe", "dll"];
    if media.contains(&ext.as_str()) {
        return format!(
            "「{}」是二进制/媒体文件（{}），不是文本，别用 read_file/read_page 读。图片请让用户直接发图（走多模态），文档类用对应的文档处理技能。",
            full.display(),
            ext
        );
    }
    format!("读文件失败（可能非 UTF-8）: {e}")
}

pub(crate) fn read_file(ws: &Path, args: &str, max_chars: usize, unconfined: bool) -> Result<String, String> {    let v: Value = serde_json::from_str(args).map_err(|e| format!("参数解析失败: {e}"))?;
    let rel = v.get("path").and_then(Value::as_str).ok_or("缺少 path 参数")?;
    let full = resolve_in_workspace(ws, rel, true, unconfined)?;
    let meta = full.metadata().map_err(|e| format!("读取失败: {e}"))?;
    if meta.is_dir() {
        return Err("目标是目录，不是文件".to_string());
    }
    let text = std::fs::read_to_string(&full).map_err(|e| read_text_err(&full, e))?;
    Ok(cap(&text, max_chars))
}

/// 分页读取大文件（P5）：按行分页，页码越界自动钳到最后/第一页。
pub(crate) fn read_page(ws: &Path, args: &str, max_chars: usize, unconfined: bool) -> Result<String, String> {
    let v: Value = serde_json::from_str(args).map_err(|e| format!("参数解析失败: {e}"))?;
    let rel = v.get("path").and_then(Value::as_str).ok_or("缺少 path 参数")?;
    let page = v.get("page").and_then(Value::as_u64).ok_or("缺少 page 参数")?;
    if page == 0 {
        return Err("page 从 1 开始".to_string());
    }
    let page_size = v.get("page_size").and_then(Value::as_u64).unwrap_or(200).clamp(10, 1000) as usize;
    let full = resolve_in_workspace(ws, rel, true, unconfined)?;
    if full.metadata().map_err(|e| format!("读取失败: {e}"))?.is_dir() {
        return Err("目标是目录，不是文件".to_string());
    }
    let text = std::fs::read_to_string(&full).map_err(|e| read_text_err(&full, e))?;
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return Ok(format!("{rel} 是空文件"));
    }
    let pages = lines.len().div_ceil(page_size);
    let page = (page as usize).min(pages);
    let start = (page - 1) * page_size;
    let end = (start + page_size).min(lines.len());
    let body = lines[start..end].join("\n");
    let out = format!(
        "{} 第 {page}/{pages} 页（共 {} 行，本页 {}-{} 行）\n{}",
        rel,
        lines.len(),
        start + 1,
        end,
        body
    );
    Ok(cap(&out, max_chars))
}

/// 文件类工具的统一体积上限：`write_file` 的 content、`edit_file` 的目标文件与编辑结果
/// 共用（审查 N2——edit_file 原先无上限，模型误编辑大文件时整文件读入内存）。
pub(crate) const MAX_FILE_BYTES: usize = 512 * 1024;

pub(crate) fn write_file(ws: &Path, args: &str, unconfined: bool) -> Result<String, String> {
    let v: Value = serde_json::from_str(args).map_err(|e| format!("参数解析失败: {e}"))?;
    let rel = v.get("path").and_then(Value::as_str).ok_or("缺少 path 参数")?;
    let content = v.get("content").and_then(Value::as_str).ok_or("缺少 content 参数")?;
    if content.len() > MAX_FILE_BYTES {
        return Err(format!("内容过大（{} 字节，上限 512KB）", content.len()));
    }
    let full = resolve_in_workspace(ws, rel, false, unconfined)?;
    // 原子写：写到一半失败不能留下半截文件（模型写代码文件时这是最常见的损坏来源）
    crate::fsx::write_atomic_str(&full, content).map_err(|e| format!("写文件失败: {e}"))?;
    Ok(format!("已写入 {} 字节 → {}", content.len(), rel))
}

/// 局部修改（P16）：文件里唯一出现的 old_string → new_string。
/// 大文件改几行用它，省 token 也避免重写全文时误伤其他内容。
pub(crate) fn edit_file(ws: &Path, args: &str, unconfined: bool) -> Result<String, String> {
    let v: Value = serde_json::from_str(args).map_err(|e| format!("参数解析失败: {e}"))?;
    let rel = v.get("path").and_then(Value::as_str).ok_or("缺少 path 参数")?;
    let old = v.get("old_string").and_then(Value::as_str).ok_or("缺少 old_string 参数")?;
    let new = v.get("new_string").and_then(Value::as_str).ok_or("缺少 new_string 参数")?;
    let replace_all = v.get("replace_all").and_then(Value::as_bool).unwrap_or(false);
    if old.is_empty() {
        return Err("old_string 为空：本工具做定位替换，不接受空串（整文件重写请用 write_file）".to_string());
    }
    if old == new {
        return Err("old_string 与 new_string 相同，无需修改".to_string());
    }
    if new.len() > MAX_FILE_BYTES {
        return Err(format!("new_string 过大（{} 字节，上限 512KB）", new.len()));
    }
    let full = resolve_in_workspace(ws, rel, true, unconfined)?;
    let meta = full.metadata().map_err(|e| format!("读取失败: {e}"))?;
    if meta.is_dir() {
        return Err("目标是目录，不是文件".to_string());
    }
    if meta.len() > MAX_FILE_BYTES as u64 {
        return Err(format!(
            "文件过大（{} 字节，上限 512KB）：edit_file 只用于改文本片段，拒绝整文件读入",
            meta.len()
        ));
    }
    let text = std::fs::read_to_string(&full).map_err(|e| format!("读文件失败（可能非 UTF-8）: {e}"))?;
    let (out, count, line) = apply_edit(&text, old, new, replace_all, rel)?;
    if out.len() > MAX_FILE_BYTES {
        return Err(format!(
            "编辑后文件将达 {} 字节，超过 512KB 上限（原 {} 字节）：请缩小改动或用 write_file 分文件写",
            out.len(),
            text.len()
        ));
    }
    // 与其他落盘点同一原子写：替换一半失败不留半截文件
    crate::fsx::write_atomic_str(&full, &out).map_err(|e| format!("写文件失败: {e}"))?;
    Ok(format!(
        "已编辑 {rel}：{count} 处替换（首处在第 {line} 行，{} → {} 字节）",
        text.len(),
        out.len()
    ))
}

/// 定位替换的纯逻辑（可单测）：返回（替换后全文, 替换处数, 首处行号）。
///
/// CRLF 容错：文件是**纯 CRLF** 行尾而 old_string 按 LF 书写时（模型最常见的输入形态），
/// 把全文按 LF 规范化后匹配、替换，再整体还原 CRLF（往返可逆才做，混合行尾不猜测）。
pub(crate) fn apply_edit(
    text: &str,
    old: &str,
    new: &str,
    replace_all: bool,
    rel: &str,
) -> Result<(String, usize, usize), String> {
    let mut work = text.to_string();
    let mut w_old = old.to_string();
    let mut w_new = new.to_string();
    let mut crlf = false;
    if text.matches(old).count() == 0 && text.contains("\r\n") && !old.contains('\r') {
        let lf = text.replace("\r\n", "\n");
        // 纯 CRLF（往返一致）且规范化后能命中，才走这条宽容路径
        if lf.replace('\n', "\r\n") == text && lf.contains(old) {
            w_new = new.replace("\r\n", "\n");
            work = lf;
            w_old = old.to_string();
            crlf = true;
        }
    }
    let n = work.matches(w_old.as_str()).count();
    if n == 0 {
        return Err(format!(
            "old_string 未命中 {rel}{}：old_string 必须与文件内容逐字一致（含缩进、空行与换行），先 read_file/read_page 取准确片段再试",
            near_line_hint(&work, old)
        ));
    }
    if n > 1 && !replace_all {
        return Err(format!(
            "old_string 在 {rel} 中出现 {n} 次，不唯一：上下各多带几行上下文让它唯一，或置 replace_all=true 全部替换"
        ));
    }
    let first = work.find(w_old.as_str()).unwrap_or(0);
    let line = work[..first].matches('\n').count() + 1;
    let out = if replace_all {
        work.replace(w_old.as_str(), w_new.as_str())
    } else {
        work.replacen(w_old.as_str(), w_new.as_str(), 1)
    };
    let out = if crlf { out.replace('\n', "\r\n") } else { out };
    Ok((out, if replace_all { n } else { 1 }, line))
}

/// 未命中时的定位提示：old_string 的首个非空行若在文件中出现，报出它所在行号，
/// 让模型知道「差不多的位置在哪、多半是缩进或空行对不上」。
pub(crate) fn near_line_hint(text: &str, old: &str) -> String {
    let head = old.lines().map(str::trim).find(|l| l.len() >= 8).unwrap_or("");
    if head.is_empty() {
        return String::new();
    }
    match text.find(head) {
        Some(pos) => {
            let line = text[..pos].matches('\n').count() + 1;
            format!("（old_string 首行「{head}」出现在第 {line} 行附近，多半是缩进/空行/相邻行对不上）")
        }
        None => String::new(),
    }
}
