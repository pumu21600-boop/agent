//! 屏幕采集：截图（GDI BitBlt + GetDIBits → 手写 PNG）、光标位置、屏幕尺寸。
//!
//! 取的是**虚拟屏**（多显示器拼起来的整块，SM_XVIRTUALSCREEN 起点）——与
//! SendInput 的绝对坐标空间（0..65535 映射虚拟屏）同系，截图上的 (x,y) 就是
//! 键鼠点击的 (x,y)，不用换算。

use std::os::raw::c_void;

use windows_sys::Win32::Foundation::POINT;
use windows_sys::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
    ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, SRCCOPY,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CXSCREEN, SM_CYVIRTUALSCREEN, SM_CYSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

/// 虚拟屏几何：(x, y, width, height)。多屏拼接，左上角可能是负数。
pub fn virtual_screen() -> (i32, i32, u32, u32) {
    unsafe {
        let x = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let y = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let w = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let h = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        if w <= 0 || h <= 0 {
            // 极端环境（远程桌面异常态）退主屏
            (0, 0, GetSystemMetrics(SM_CXSCREEN) as u32, GetSystemMetrics(SM_CYSCREEN) as u32)
        } else {
            (x, y, w as u32, h as u32)
        }
    }
}

/// 光标位置（屏幕坐标；取不到返回 None）。
pub fn cursor_pos() -> Option<(i32, i32)> {
    unsafe {
        let mut p = POINT { x: 0, y: 0 };
        (GetCursorPos(&mut p) != 0).then_some((p.x, p.y))
    }
}

/// 截一张虚拟屏，返回 (BGRA 原始像素, width, height, stride)。
///
/// GDI 链：桌面 DC → 兼容 DC/位图 → BitBlt 拷贝 → GetDIBits 出 32bpp BGRA
/// （biHeight 取负 = top-down，免去垂直翻转）。所有 GDI 对象在离开前释放
/// （泄露一个 DC 就少一分 GDI 配额，长跑会耗尽）。PNG 编码与降采样交给
/// 调用方（它要同时出全尺寸与内联两份）。
pub fn capture_bgra() -> Result<(Vec<u8>, u32, u32, usize), String> {
    unsafe {
        let (vx, vy, w, h) = virtual_screen();
        if w == 0 || h == 0 {
            return Err("屏幕尺寸为 0（远程桌面/无头环境？）".into());
        }
        let screen_dc = GetDC(0);
        if screen_dc == 0 {
            return Err("取桌面 DC 失败".into());
        }
        let mem_dc = CreateCompatibleDC(screen_dc);
        if mem_dc == 0 {
            ReleaseDC(0, screen_dc);
            return Err("建兼容 DC 失败".into());
        }
        let bitmap = CreateCompatibleBitmap(screen_dc, w as i32, h as i32);
        if bitmap == 0 {
            DeleteDC(mem_dc);
            ReleaseDC(0, screen_dc);
            return Err("建兼容位图失败".into());
        }
        let old = SelectObject(mem_dc, bitmap);
        // 整屏拷贝（SRCCOPY）。锁屏/安全桌面下 BitBlt 给出全黑——不报错，
        // 由调用方看内容判断（全屏同色时人话提示）。
        let blit_ok = BitBlt(
            mem_dc,
            0,
            0,
            w as i32,
            h as i32,
            screen_dc,
            vx,
            vy,
            SRCCOPY,
        ) != 0;
        if !blit_ok {
            SelectObject(mem_dc, old);
            DeleteObject(bitmap);
            DeleteDC(mem_dc);
            ReleaseDC(0, screen_dc);
            return Err("BitBlt 拷屏失败（锁屏/安全桌面？）".into());
        }
        // GetDIBits：32bpp BI_RGB，top-down（biHeight 负）
        let row_bytes = (w as usize) * 4;
        let mut buf = vec![0u8; row_bytes * h as usize];
        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0, // BI_RGB
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            bmiColors: [std::mem::zeroed()],
        };
        let scanned = GetDIBits(
            mem_dc,
            bitmap,
            0,
            h,
            buf.as_mut_ptr() as *mut c_void,
            &mut bmi,
            DIB_RGB_COLORS,
        );
        SelectObject(mem_dc, old);
        DeleteObject(bitmap);
        DeleteDC(mem_dc);
        ReleaseDC(0, screen_dc);
        if scanned == 0 {
            return Err("GetDIBits 读像素失败".into());
        }
        Ok((buf, w, h, row_bytes))
    }
}

/// 全屏同色判定（锁屏/黑屏时给一句人话，别让模型对一张全黑图发呆）。
pub fn is_solid_color(bgra: &[u8]) -> bool {
    let Some(first) = bgra.get(..4) else { return true };
    bgra.as_chunks::<4>().0.iter().all(|px| *px == first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solid_color_detection() {
        assert!(is_solid_color(&[0, 0, 0, 255, 0, 0, 0, 255]), "全黑=同色");
        assert!(!is_solid_color(&[0, 0, 0, 255, 1, 0, 0, 255]), "有差异=非同色");
        assert!(is_solid_color(&[]), "空缓冲按同色处理（调用方另行判尺寸）");
    }
}
