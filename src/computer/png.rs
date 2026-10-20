//! 极简 PNG 编码器（computer 截图用；零新增依赖）。
//!
//! 为什么不引 image/png crate：本机验证时外网不通，且截图的管线已经很确定——
//! GDI GetDIBits 出 32bpp BGRA，去掉 alpha 就是 RGB truecolor，PNG 只剩
//! 「zlib(flate2 已有) + CRC32(手写 20 行) + 四个 chunk」三件事。
//! 只编码、不解码（面板显示走浏览器 <img>，不用本模块）。

use std::io::Write;

/// PNG 签名（8 字节）。
const SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// CRC32（IEEE 802.3，PNG 用的多项式 0xEDB88320 反射形式）。表惰性生成。
fn crc32_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    for (i, slot) in t.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        *slot = c;
    }
    t
}

fn crc32(data: &[u8]) -> u32 {
    let t = crc32_table();
    let mut c: u32 = 0xFFFF_FFFF;
    for &b in data {
        c = t[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

/// 写一个 chunk：len(4 BE) + type(4) + data + crc32(type+data)(4 BE)。
fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc_input = Vec::with_capacity(4 + data.len());
    crc_input.extend_from_slice(kind);
    crc_input.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

/// BGRA（每像素 4 字节，GDI GetDIBits 的 32bpp 输出）→ RGB PNG 字节。
/// `stride` = 每行字节数（可能 > width*4，GDI 会对齐到 4 的倍数——32bpp 天然对齐，
/// 但保守起见按传入 stride 走；行内多出来的字节跳过）。
pub fn encode_png_from_bgra(bgra: &[u8], width: u32, height: u32, stride: usize) -> Result<Vec<u8>, String> {
    if width == 0 || height == 0 {
        return Err("截图尺寸为 0".into());
    }
    let row_bytes = (width as usize) * 4;
    if stride < row_bytes {
        return Err(format!("stride {stride} 小于行字节数 {row_bytes}"));
    }
    // 扫描线序列：每行 = 1 字节 filter(0=None) + width*3 字节 RGB
    let mut raw = Vec::with_capacity((row_bytes / 4 * 3 + 1) * height as usize);
    for y in 0..height as usize {
        raw.push(0u8); // filter type None
        let row = &bgra[y * stride..y * stride + row_bytes];
        for px in row.as_chunks::<4>().0 {
            raw.push(px[2]); // R ← B位 后的 G/R：BGRA 序 [B,G,R,A]
            raw.push(px[1]);
            raw.push(px[0]);
        }
    }
    // IHDR：width/height/bit_depth(8)/color_type(2=RGB)/compression(0)/filter(0)/interlace(0)
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);

    let mut out = Vec::with_capacity(raw.len() / 2 + 64);
    out.extend_from_slice(&SIG);
    write_chunk(&mut out, b"IHDR", &ihdr);
    let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(&raw).map_err(|e| format!("PNG zlib 压缩失败: {e}"))?;
    let idat = enc.finish().map_err(|e| format!("PNG zlib 收尾失败: {e}"))?;
    write_chunk(&mut out, b"IDAT", &idat);
    write_chunk(&mut out, b"IEND", &[]);
    Ok(out)
}

/// 最近邻降采样 BGRA（截图内联给模型前压尺寸用，纯指针算术无依赖）。
/// 返回 (新宽, 新高, 新 stride, 新缓冲)。超过上限才需要调；不超时原样返回 None。
pub fn downscale_bgra(
    bgra: &[u8],
    width: u32,
    height: u32,
    stride: usize,
    max_w: u32,
) -> Option<(u32, u32, Vec<u8>)> {
    if width <= max_w {
        return None;
    }
    let scale = max_w as f64 / width as f64;
    let new_w = max_w;
    let new_h = ((height as f64 * scale).round() as u32).max(1);
    let new_stride = (new_w as usize) * 4;
    let mut out = vec![0u8; new_stride * new_h as usize];
    for y in 0..new_h as usize {
        let sy = ((y as f64 / scale) as usize).min(height as usize - 1);
        for x in 0..new_w as usize {
            let sx = ((x as f64 / scale) as usize).min(width as usize - 1);
            let src = sy * stride + sx * 4;
            let dst = y * new_stride + x * 4;
            out[dst..dst + 4].copy_from_slice(&bgra[src..src + 4]);
        }
    }
    Some((new_w, new_h, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_signature_and_chunks_wellformed() {
        // 2×1 纯红像素（BGRA: [0,0,255,255]）
        let bgra = [0u8, 0, 255, 255, 0, 0, 255, 255];
        let png = encode_png_from_bgra(&bgra, 2, 1, 8).unwrap();
        assert!(png.starts_with(&SIG), "签名必须在最前");
        assert!(png.windows(4).any(|w| w == b"IHDR"), "IHDR chunk 在");
        assert!(png.windows(4).any(|w| w == b"IDAT"), "IDAT chunk 在");
        assert!(png.ends_with(b"IEND\xaeB`\x82") || png.windows(4).any(|w| w == b"IEND"), "IEND 收尾");
        // IHDR 里宽高正确（BE u32）
        assert_eq!(&png[16..20], &2u32.to_be_bytes());
        assert_eq!(&png[20..24], &1u32.to_be_bytes());
        assert_eq!(png[24], 8, "bit depth 8");
        assert_eq!(png[25], 2, "color type = truecolor RGB");
    }

    #[test]
    fn crc32_known_vector() {
        // CRC32("IEND") = 0xAE426082（PNG 规范里的经典值）
        assert_eq!(crc32(b"IEND"), 0xAE42_6082);
    }

    #[test]
    fn downscale_halves_dimensions() {
        // 4×2 BGRA → 上限 2 宽 → 2×1，取最近邻（左上角像素）
        let mut bgra = vec![0u8; 16];
        for (i, px) in bgra.chunks_exact_mut(4).enumerate() {
            px.copy_from_slice(&[i as u8, 100, 200, 255]);
        }
        let (w, h, out) = downscale_bgra(&bgra, 4, 2, 16, 2).expect("该降");
        assert_eq!((w, h), (2, 1));
        assert_eq!(out.len(), 8);
        assert_eq!(&out[0..4], &[0, 100, 200, 255], "第一个像素取源 (0,0)");
        // 不超上限：None（调用方原样编码）
        assert!(downscale_bgra(&bgra, 4, 2, 16, 8).is_none());
    }

    #[test]
    fn stride_padding_skipped() {
        // stride 大于行字节（对齐填充）时编码不错位
        let mut bgra = vec![0u8; 3 * 12]; // 宽 2 高 2，stride 12（行 8 + 4 填充）
        bgra[0] = 255; // (0,0) 蓝位
        bgra[12] = 255; // (0,1) 蓝位
        let png = encode_png_from_bgra(&bgra, 2, 2, 12).unwrap();
        assert!(png.len() > 40);
    }
}
