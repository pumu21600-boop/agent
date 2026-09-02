//! 沙箱内存探针用的原生内存 hog —— 施工单 004
//!
//! 用法：sandbox_hog.exe <MB>  按 64MB 块 VirtualAlloc(MEM_COMMIT) 分配指定内存，
//! 全部分完打印 HOG-ALLOC-OK 退出 0；中途分配失败打印 HOG-OOM-AT <已分配MB> 退出 1。
//! 纯原生、无 .NET 依赖，避免 powershell 在受限 token 下的 0xC0000142 干扰探针结论。

use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};

const CHUNK_MB: usize = 64;

fn main() {
    let target_mb: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(64);
    let mut got_mb = 0usize;
    while got_mb < target_mb {
        let chunk = CHUNK_MB.min(target_mb - got_mb);
        let p = unsafe {
            VirtualAlloc(
                std::ptr::null_mut(),
                chunk * 1024 * 1024,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        };
        if p.is_null() {
            eprintln!("HOG-OOM-AT {got_mb}");
            std::process::exit(1);
        }
        // 摸一下首字节，防任何延迟提交
        unsafe { *(p as *mut u8) = 1 };
        got_mb += chunk;
    }
    println!("HOG-ALLOC-OK {got_mb}");
}
