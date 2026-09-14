// 记忆库星云的 3D 布局：项目 = 星云团（团心），会话 = 团里的恒星。
// 全部确定性（id/项目名哈希），刷新、重开都不跳位。

export const HUES = [198, 262, 322, 168, 44, 220, 288, 12, 186, 340];

// 默认机位（进场/复位/宽屏拉伸轴都以此为基准），Galaxy.svelte 相机初始角与此保持一致
export const DEFAULT_AZ = 3.6;
export const DEFAULT_EL = 0.25;

export function hashStr(s) {
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return h >>> 0;
}

export function mulberry32(a) {
  return function () {
    a |= 0;
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

/** 标准正态（Box-Muller） */
function gauss(rnd) {
  const g1 = Math.sqrt(-2 * Math.log(rnd() || 1e-6)) * Math.cos(2 * Math.PI * rnd());
  const g2 = Math.sqrt(-2 * Math.log(rnd() || 1e-6)) * Math.sin(2 * Math.PI * rnd());
  return [g1, g2];
}

/**
 * items → { clusters, stars, center, radius }
 * 团心按会话数从多到少排：最大的居中，其余沿黄金角螺旋外扩，
 * 步进按前一个团的实际大小递推，避免小团被大团的体积吞掉。
 */
export function buildScene(items, opts = {}) {
  const byProj = new Map();
  for (const it of items) {
    const p = it.project || '未分组';
    if (!byProj.has(p)) byProj.set(p, []);
    byProj.get(p).push(it);
  }
  const names = [...byProj.keys()].sort(
    (a, b) => byProj.get(b).length - byProj.get(a).length || a.localeCompare(b, 'zh'),
  );
  let acc = 0;
  let prevSigma = 0;
  const clusters = names.map((name, i) => {
    const count = byProj.get(name).length;
    const sigma = 42 + 20 * Math.sqrt(count);
    let x = 0;
    let z = 0;
    if (i > 0) {
      // 半径按"前一个团的体量"递推，但至少按 sqrt(i) 外扩——避免一堆 1 条的
      // 小项目被线性累加推到很远，整片星云散成孤岛。
      // P14 十轮收紧：之前 1.5/130/460 的间距把场景撑得太空旷（用户："空间太大"）
      acc = Math.max(acc + prevSigma * 1.15 + 90, 330 * Math.sqrt(i));
      const ang = i * 2.399963229728653;
      x = Math.cos(ang) * acc;
      z = Math.sin(ang) * acc;
    }
    prevSigma = sigma;
    const rnd = mulberry32(hashStr(`cluster${name}`));
    return {
      name,
      center: [x, (rnd() - 0.5) * 340, z],
      hue: HUES[hashStr(name) % HUES.length],
      count,
      sigma,
    };
  });
  const cmap = new Map(clusters.map((c) => [c.name, c]));
  // 光点全局随机分布（用户要求：拉到最远时光点铺满全画面，不是挤在局部）：
  // 以场景中心为原点的大半径均匀球壳，项目只用星色区分。
  // fieldR 由团心覆盖范围估算，保证散布盖住所有团的领空。
  // 集聚程度（用户要求滑条可调）：径向指数向心压缩——最大半径几乎不变
  // （远景相机距离稳定、光点不消失），主体随程度拉向中心；
  // =1 时接近最初的高斯团（用户："集中在中间，和一开始那样"）
  const cluster = Math.min(1, Math.max(0, opts.cluster || 0));
  let maxCenter = 400;
  for (const c of clusters) {
    maxCenter = Math.max(maxCenter, Math.hypot(c.center[0], c.center[2]));
  }
  let fieldR = maxCenter * 1.15 + 500;
  // 对话上下文长度因子（用户要求：上下文越长光点越大越亮）：按全部消息字符数走
  // 对数刻度，再按当前数据集的 25/90 分位归一——任何规模的库都能拉开可见的
  // 明暗大小层次；后端未带 chars 的旧索引按消息数×120 估算
  const ests = items.map((x) => (x.chars > 0 ? x.chars : (x.count || 1) * 120));
  const logs = ests.map((e) => Math.log10(Math.max(1, e))).sort((a, b) => a - b);
  const lo = logs[Math.floor(logs.length * 0.25)] ?? 3;
  const hi = Math.max(lo + 0.8, logs[Math.floor(logs.length * 0.9)] ?? 5);
  const ctxOf = (item) => {
    const est = item.chars > 0 ? item.chars : (item.count || 1) * 120;
    return Math.min(1, Math.max(0, (Math.log10(Math.max(1, est)) - lo) / (hi - lo)));
  };
  const stars = [];
  for (const it of items) {
    const c = cmap.get(it.project || '未分组');
    const rnd = mulberry32(hashStr(it.id));
    // 球体积均匀采样（高斯会向中心聚堆——用户："分布还是不够散"）：
    // 方向均匀 + 半径立方根均匀，星星均匀铺满整个星云场直到外围。
    // 先放在"未集聚"的散布位（radius 基准），集聚在后面统一向心缩放
    const th = rnd() * Math.PI * 2;
    const u = rnd() * 2 - 1;
    const spread = 0.3 + 0.7 * Math.cbrt(rnd());
    const sxy = Math.sqrt(1 - u * u);
    const ctx = ctxOf(it);
    stars.push({
      ...it,
      cluster: c.name,
      hue: c.hue,
      sigma: c.sigma,
      center: c.center,
      pos: [
        fieldR * spread * sxy * Math.cos(th),
        fieldR * spread * u * 0.85,
        fieldR * spread * sxy * Math.sin(th),
      ],
      spread,
      phase: rnd(),
      // 对话上下文越长，光点越大越亮（用户要求，两次追调："整体再提高、不要保守"、"还是不够"）
      size: 3.2 + 8.5 * ctx,
      glow: 0.7 + 0.6 * ctx,
    });
  }
  // 场景半径取"未集聚"基准：巨壳半径/合成缩放/背景星壳都以它为参数，
  // 若跟着集聚档位缩放，整片背景星云会跟着缩放平移（用户实拍"星云跟着集聚"）
  let radius = 400;
  for (const st of stars) {
    const d = Math.hypot(st.pos[0], st.pos[1], st.pos[2]) + st.sigma * 2.2;
    radius = Math.max(radius, d);
  }
  // 集聚（用户要求滑条可调）：整体位置向量向原点等比缩放（方向不变），
  // 最大半径几乎不变（远景相机距离稳定、光点不消失）；=1 时接近最初的高斯团
  if (cluster > 0.001) {
    for (const st of stars) {
      const f = Math.pow(st.spread, 3.2 * cluster);
      st.pos = [st.pos[0] * f, st.pos[1] * f, st.pos[2] * f];
    }
  }
  // 宽屏铺开（用户："极限远距离下光点左右铺开一点"）：球形点云从远处投影成圆盘，
  // 宽幅画面左右必然留空。沿默认机位的屏幕横轴把光点拉伸 s=宽高比（上限 2.4），
  // 配合 Galaxy.svelte 逐星各向异性 fit，远景左右也能顶满画面。确定性不受影响。
  // 集聚程度拉高时同步收回拉伸：=1 回到最初的圆形团。
  // 只拉伸光点：背景星云团心保持原位，不随集聚/铺开变化（用户要求）
  const aspect = opts.aspect || 1.6;
  const stretch = 1 + (Math.min(2.4, Math.max(1, aspect)) - 1) * (1 - cluster);
  if (stretch > 1.001) {
    // 默认机位屏幕横轴 r0 = norm(cross(f0, [0,1,0])) = (sin az0, 0, -cos az0)，只取水平分量
    const ux = Math.sin(DEFAULT_AZ);
    const uz = -Math.cos(DEFAULT_AZ);
    const k = stretch - 1;
    for (const st of stars) {
      const t = k * (st.pos[0] * ux + st.pos[2] * uz);
      st.pos = [st.pos[0] + ux * t, st.pos[1], st.pos[2] + uz * t];
    }
    let maxXZ = 400;
    for (const st of stars) maxXZ = Math.max(maxXZ, Math.hypot(st.pos[0], st.pos[2]));
    fieldR = Math.max(fieldR, maxXZ * 1.02);
  }
  return { clusters, stars, center: [0, 0, 0], radius, fieldR };
}
