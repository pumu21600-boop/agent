// 生成星云噪声纹理：4 个通道 = 4 种不同的基函数（可平铺、64³ RGBA8）。
//   R = fbm 体噪声（低频云团）
//   G = ridged 多重分形（1-|2n-1|² 带权重递推）→ 锐利的脊络（细丝"骨相"）
//   B = 中频单脊 1-|2·fbm-1| → 长而蜿蜒的折痕线
//   A = 高频单脊 → 细碎毛刺
// 值噪声天生"棉花"，但 abs 折痕是 V 形锐利的——烘焙进纹理后经三线性插值仍是细丝。
// 着色器一次 texture() 同时拿到四种基，自由组合出"体 + 丝 + 尘"。

function mulberry32(a) {
  return function () {
    a |= 0;
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

/** 一个八度：周期晶格 L（保证可平铺），三线性 + smoothstep 插值 */
function octave(size, L, rnd) {
  const vals = new Float32Array(L * L * L);
  for (let i = 0; i < vals.length; i++) vals[i] = rnd();
  const at = (x, y, z) => vals[((z % L) * L + (y % L)) * L + (x % L)];
  const out = new Float32Array(size * size * size);
  const step = L / size;
  for (let z = 0; z < size; z++) {
    const fz = z * step;
    const z0 = Math.floor(fz);
    const tz = fz - z0;
    const wz = tz * tz * (3 - 2 * tz);
    for (let y = 0; y < size; y++) {
      const fy = y * step;
      const y0 = Math.floor(fy);
      const ty = fy - y0;
      const wy = ty * ty * (3 - 2 * ty);
      let idx = (z * size + y) * size;
      for (let x = 0; x < size; x++) {
        const fx = x * step;
        const x0 = Math.floor(fx);
        const tx = fx - x0;
        const wx = tx * tx * (3 - 2 * tx);
        const x1 = x0 + 1;
        const y1 = y0 + 1;
        const z1 = z0 + 1;
        const c000 = at(x0, y0, z0);
        const c100 = at(x1, y0, z0);
        const c010 = at(x0, y1, z0);
        const c110 = at(x1, y1, z0);
        const c001 = at(x0, y0, z1);
        const c101 = at(x1, y0, z1);
        const c011 = at(x0, y1, z1);
        const c111 = at(x1, y1, z1);
        const a = c000 + (c100 - c000) * wx;
        const b = c010 + (c110 - c010) * wx;
        const c = c001 + (c101 - c001) * wx;
        const d = c011 + (c111 - c011) * wx;
        out[idx++] = a + (b - a) * wy + (c + (d - c) * wy - (a + (b - a) * wy)) * wz;
      }
    }
  }
  return out;
}

export function makeNoiseTextureData(size = 64, seed = 20260911) {
  const rnd = mulberry32(seed);
  // 四层基础值噪声（晶格 4/8/16/32，全可平铺）
  const o = [octave(size, 4, rnd), octave(size, 8, rnd), octave(size, 16, rnd), octave(size, 32, rnd)];
  const n = size * size * size;
  const data = new Uint8Array(n * 4);
  for (let i = 0; i < n; i++) {
    const v1 = o[0][i];
    const v2 = o[1][i];
    const v3 = o[2][i];
    const v4 = o[3][i];
    // R：fbm 体
    const r = v1 * 0.5 + v2 * 0.25 + v3 * 0.125 + v4 * 0.0625;
    // G：ridged 多重分形（锐利脊络）
    let g = 0;
    let amp = 0.62;
    let w = 1.0;
    const vs = [v1, v2, v3, v4];
    for (let k = 0; k < 4; k++) {
      let rid = 1 - Math.abs(2 * vs[k] - 1);
      rid *= rid * w;
      w = Math.min(1, rid * 1.9);
      g += rid * amp;
      amp *= 0.56;
    }
    // B：中频脊（v1+v2 组合 → 长蜿蜒折痕）
    const b = 1 - Math.abs(2 * (v1 * 0.62 + v2 * 0.38) - 1);
    // A：高频脊（v3+v4 组合 → 细碎毛刺）
    const a = 1 - Math.abs(2 * (v3 * 0.55 + v4 * 0.45) - 1);
    data[i * 4 + 0] = (Math.min(1, r) * 255) | 0;
    data[i * 4 + 1] = (Math.min(1, g * 1.15) * 255) | 0;
    data[i * 4 + 2] = (b * 255) | 0;
    data[i * 4 + 3] = (a * 255) | 0;
  }
  return { data, size };
}
