//! 文件系统小工具。
//!
//! `write_atomic`：**同目录临时文件 + 原子替换**。`std::fs::write` 是「打开→截断→写」，
//! 写到一半失败（磁盘满 / 进程被杀 / 断电）会留下半截文件——写用户代码文件会把代码写坏，
//! 写 settings.json 会把配置和 key 写坏。同卷 rename 在 Windows 上走
//! `MoveFileExW(REPLACE_EXISTING)`：要么还是旧文件、要么已经是完整新文件，不存在中间态。

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// 原子写：内容先落到同目录临时文件，再 rename 覆盖目标。
///
/// 临时名带进程号与自增序号，避免同目录并发写互相踩；失败路径负责清理临时文件。
/// 目标父目录不存在时按 `create_dir_all` 建（与 write_file 工具的既有语义一致）。
pub fn write_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => PathBuf::from("."),
    };
    std::fs::create_dir_all(&dir)?;
    let stem = path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "file".into());
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".{stem}.tmp-{}-{seq}", std::process::id()));

    if let Err(e) = std::fs::write(&tmp, contents) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    // 2026-10-08 修：rename 只保证目录项的原子切换，不保证数据块已落盘——硬杀/掉电
    // 时可能出现「rename 已生效、内容没写下去」的半截文件（模块头把「断电」列为已
    // 防护场景，实际差这一步）。fsync 临时文件把数据推下去再 rename。
    if let Err(e) = fsync_file(&tmp) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// fsync 一个文件（数据落盘）。失败不致命（某些文件系统/网络盘不支持），
/// 由调用方决定是否清理临时文件——这里返回 Err 让 write_atomic 按失败路径收口。
fn fsync_file(path: &Path) -> std::io::Result<()> {
    let f = std::fs::OpenOptions::new().write(true).open(path)?;
    f.sync_all()?;
    drop(f);
    // 目录项也要落盘，否则极端情况下 rename 本身可能丢（网络文件系统上常常失败，
    // 不阻断主流程）
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
        && let Ok(d) = std::fs::OpenOptions::new().read(true).open(dir)
    {
        let _ = d.sync_all();
    }
    Ok(())
}

/// 文本便捷入口（UTF-8）。
pub fn write_atomic_str(path: &Path, contents: &str) -> std::io::Result<()> {
    write_atomic(path, contents.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("agent-fsx-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn replaces_existing_and_leaves_no_temp() {
        let d = tmpdir("replace");
        let f = d.join("a.txt");
        std::fs::write(&f, "旧内容旧内容旧内容").unwrap();
        write_atomic_str(&f, "新").unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "新", "旧内容必须被完整替换（不残留尾部）");
        // 临时文件不许留痕
        let leftovers: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "临时文件未清理: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn creates_parent_dirs_and_handles_binary() {
        let d = tmpdir("nested");
        let f = d.join("sub").join("deep").join("b.bin");
        let bytes: Vec<u8> = (0u8..=255).collect();
        write_atomic(&f, &bytes).unwrap();
        assert_eq!(std::fs::read(&f).unwrap(), bytes);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 写失败不得破坏已有文件：目标路径的父目录被换成文件（必然失败），但既有文件仍在原处。
    #[test]
    fn failure_keeps_old_file_intact() {
        let d = tmpdir("keep");
        let good = d.join("keep.txt");
        std::fs::write(&good, "完好").unwrap();
        // 直接给一个不可能写入的路径（父目录是文件）
        let blocked = good.join("child.txt");
        assert!(write_atomic_str(&blocked, "x").is_err(), "父路径是文件时必须报错");
        assert_eq!(std::fs::read_to_string(&good).unwrap(), "完好", "原有文件不能被牵连");
        let _ = std::fs::remove_dir_all(&d);
    }
}
