<script>
  // 记忆库星云（体积渲染版）：星云 = WebGL2 光线步进的体积分形，恒星 = 会话。
  // 数据/检索走 /api/memory/*；渲染委托给 galaxy/renderer.js；标签与选中环走 2D 覆盖层。
  import { onMount, onDestroy, createEventDispatcher, tick } from 'svelte';
  import { buildScene, mulberry32, DEFAULT_AZ, DEFAULT_EL } from './galaxy/layout.js';
  import { NebulaRenderer } from './galaxy/renderer.js';

  const dispatch = createEventDispatcher();
  export let currentSessionId = '';

  let glCanvas;
  let uiCanvas;
  let hostEl;
  let W = 0;
  let H = 0;
  let DPR = 1;
  let renderer = null;
  let fallback2d = false;
  let renderError = '';

  let loading = true;
  let errMsg = '';
  let items = [];
  let clusters = [];
  let stars = [];
  let sceneRadius = 600;
  let sceneFieldR = 2200; // 光点散布半径（buildScene 返回）：平移/拉远都以它为界
  let fitCenter = [0, 0, 0]; // 全体光点的包围球心（进场/复位视角用）
  let fitAllDist = 2400; // 把所有光点装进视锥所需的相机距离
  let stats = null;
  let notice = '';

  // 搜索态
  let query = '';
  let searching = false;
  let searched = false;
  let results = [];
  let matchIds = null;
  let focusProj = null;
  let searchInfo = '';
  let searchNotice = '';

  // 交互态
  let hoverId = null;
  let selected = null;
  let reindexing = false;
  let projPanelOpen = false; // 左下角项目列表：默认收起，点开才显示浮窗

  // 光点大小/亮度由对话上下文长度驱动（buildScene 给出 size/glow），不再开放滑条；
  // 星云密度固定最低档不开放（用户定档：黑场是构图的一半，稀薄才像参考照片）
  const NEBULA_DENSITY = 0.35;
  let pCluster = Math.min(1, Math.max(0, Number(localStorage.getItem('gx_p_cluster') ?? 0)));

  /** 集聚程度滑条：只重建布局（径向向心压缩 + 收回宽屏拉伸）。相机完全不动——
   *  背景星云按视线方向采样，相机挪一点整片天空就会跟着平移（用户实拍）；
   *  点云往场景中心收拢、边界光点几乎不动，当前视野始终装得下所有光点 */
  function applyCluster() {
    localStorage.setItem('gx_p_cluster', String(pCluster));
    rebuildScene();
  }

  // 相机（当前 / 目标，逐帧缓动）：球坐标绕场景中心；进场/复位用 layout.js 的默认机位
  const cam = { az: DEFAULT_AZ, el: DEFAULT_EL, dist: 1600, tx: 0, ty: 0, tz: 0 };
  const camT = { az: DEFAULT_AZ, el: DEFAULT_EL, dist: 1600, tx: 0, ty: 0, tz: 0 };
  const FOV = (48 * Math.PI) / 180;
  const TAN_FOV = Math.tan(FOV / 2);
  let dragging = false;
  let orbitDrag = false; // Shift/右键拖拽 = 环绕；默认拖拽 = 平移星云
  let dragMoved = 0;
  let lastX = 0;
  let lastY = 0;

  let raf = 0;
  let t0 = 0;
  let lastDraw = 0;

  let screenPos = new Map(); // id -> {x, y, z}（每帧更新，供拾取/标签/覆盖层用）

  function hsl(h, s, l) {
    const a = s * Math.min(l, 1 - l);
    const f = (n) => {
      const k = (n + h / 30) % 12;
      return l - a * Math.max(-1, Math.min(k - 3, 9 - k, 1));
    };
    return [f(0), f(8), f(4)];
  }

  // —— 场景：项目 = 星云团，会话 = 恒星 ——
  function buildStarList() {
    const list = [];
    // 背景星：远景球壳上的一大层暗星（参考图里的星场很密、冷暖混杂），不参与交互
    const rnd = mulberry32(99001);
    for (let i = 0; i < 3000; i++) {
      const u = rnd() * 2 - 1;
      const th = rnd() * Math.PI * 2;
      const r = sceneRadius * (1.4 + rnd() * 2.6);
      const s = Math.sqrt(1 - u * u);
      const b = 0.25 + rnd() * 0.6;
      const warm = rnd() < 0.22;
      list.push({
        pos: [Math.cos(th) * s * r, u * r, Math.sin(th) * s * r],
        size: 2.4 + rnd() * 3.4,
        color: warm ? [b, b * 0.82, b * 0.62] : [b * 0.78, b * 0.9, b],
        spike: 0,
        bright: 0.85,
        bg: true,
      });
    }
    // 会话星：小而亮的锐利光点（参考图里恒星只是点缀，云才是主角），
    // 大小/亮度由对话上下文长度驱动（布局层的 size/glow）
    for (const st of stars) {
      list.push({
        key: st.id,
        pos: st.pos,
        size: 110 + st.size * 42,
        color: hsl(st.hue, 0.85, 0.68),
        spike: 1,
        bright: st.glow,
        glow: st.glow,
      });
    }
    return list;
  }

  function rebuildScene() {
    // 宽高比传给布局：宽屏沿默认屏幕横轴拉伸光点分布，远景左右才能铺开；
    // 集聚程度控制光点向中心的径向压缩
    const sc = buildScene(items, { aspect: W > 0 && H > 0 ? W / H : 1.6, cluster: pCluster });
    clusters = sc.clusters;
    stars = sc.stars;
    sceneRadius = Math.max(320, sc.radius * 1.15);
    sceneFieldR = sc.fieldR || sceneRadius * 1.1;
    // 进场视角以"装下所有光点"为准（云缘视角会有一批光点落在相机背后/视野外）
    fitAllStars();
    if (renderer) {
      // 每个项目一团星云：位置/权重来自会话分布，hue 传真实色相（度），
      // 烘焙背景按团方向叠同色辉光——星云与数据一一对应
      renderer.setClusters(
        clusters.map((c) => ({
          pos: c.center,
          radius: Math.max(300, c.sigma * 5.2),
          weight: 0.55 + 0.45 * Math.min(1, (c.count || 1) / 12),
          hue: c.hue,
        })),
      );
      renderer.setStars(buildStarList());
    }
  }

  // —— 相机 ——
  function camPos() {
    const ce = Math.cos(cam.el);
    return [
      cam.tx + cam.dist * ce * Math.cos(cam.az),
      cam.ty + cam.dist * Math.sin(cam.el),
      cam.tz + cam.dist * ce * Math.sin(cam.az),
    ];
  }

  function norm(v) {
    const n = Math.hypot(v[0], v[1], v[2]) || 1;
    return [v[0] / n, v[1] / n, v[2] / n];
  }

  function cross(a, b) {
    return [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]];
  }

  function basis() {
    const p = camPos();
    const f = norm([cam.tx - p[0], cam.ty - p[1], cam.tz - p[2]]);
    const r = norm(cross(f, [0, 1, 0]));
    const u = cross(r, f);
    return { pos: p, fwd: f, right: r, up: u };
  }

  function project(p, b) {
    const rel = [p[0] - b.pos[0], p[1] - b.pos[1], p[2] - b.pos[2]];
    const z = rel[0] * b.fwd[0] + rel[1] * b.fwd[1] + rel[2] * b.fwd[2];
    if (z <= 1) return null;
    const x = (rel[0] * b.right[0] + rel[1] * b.right[1] + rel[2] * b.right[2]) / (z * TAN_FOV * (W / H));
    const y = (rel[0] * b.up[0] + rel[1] * b.up[1] + rel[2] * b.up[2]) / (z * TAN_FOV);
    return { x: W / 2 + x * (W / 2), y: H / 2 - y * (H / 2), z };
  }

  /** 装下半径 rad 的包围球所需相机距离：竖向半张角 TAN_FOV，横向 TAN_FOV×宽高比，
   *  窄窗口（宽<高）时横向先被裁掉，取两者中更紧的约束 */
  function fitDist(rad, pad = 1) {
    const aspect = W > 0 && H > 0 ? W / H : 1.6;
    return (rad * pad) / (TAN_FOV * Math.min(1, aspect));
  }

  function fitStars(list, pad = 1.35) {
    const pts = (list || stars).filter(Boolean);
    if (!pts.length) return;
    let cx = 0;
    let cy = 0;
    let cz = 0;
    for (const s of pts) {
      cx += s.pos[0];
      cy += s.pos[1];
      cz += s.pos[2];
    }
    cx /= pts.length;
    cy /= pts.length;
    cz /= pts.length;
    let rad = 80;
    for (const s of pts) {
      rad = Math.max(rad, Math.hypot(s.pos[0] - cx, s.pos[1] - cy, s.pos[2] - cz) + (s.sigma || 0) * 0.8);
    }
    camT.tx = cx;
    camT.ty = cy;
    camT.tz = cz;
    camT.dist = fitDist(rad, pad);
  }

  /** 进场/复位视角（= 极限远距离全景）：在默认机位下对每个光点解各向异性视锥约束——
   *  |横偏| ≤ (d+纵深)·tanH·k、|纵偏| ≤ (d+纵深)·tanV·k、且都在相机前方，取最紧的 d。
   *  画面中心对准光点包围盒中点（左右对称留边），配合布局的宽屏拉伸，
   *  椭圆点云横向顶满画面且一个不少（包围球拟合法宽屏左右会大量留空） */
  function fitAllStars() {
    let cx = 0;
    let cy = 0;
    let cz = 0;
    for (const s of stars) {
      cx += s.pos[0];
      cy += s.pos[1];
      cz += s.pos[2];
    }
    if (stars.length) {
      cx /= stars.length;
      cy /= stars.length;
      cz /= stars.length;
    }
    const ce0 = Math.cos(DEFAULT_EL);
    const f0 = [-ce0 * Math.cos(DEFAULT_AZ), -Math.sin(DEFAULT_EL), -ce0 * Math.sin(DEFAULT_AZ)];
    const r0 = [Math.sin(DEFAULT_AZ), 0, -Math.cos(DEFAULT_AZ)];
    const u0 = cross(r0, f0);
    const aspect = W > 0 && H > 0 ? W / H : 1.6;
    const k = 1 / 1.15; // 四周留 15% 边距
    const proj = [];
    let minX = 0;
    let maxX = 0;
    let minY = 0;
    let maxY = 0;
    for (const s of stars) {
      const rel = [s.pos[0] - cx, s.pos[1] - cy, s.pos[2] - cz];
      const X = rel[0] * r0[0] + rel[1] * r0[1] + rel[2] * r0[2];
      const Y = rel[0] * u0[0] + rel[1] * u0[1] + rel[2] * u0[2];
      const Z = rel[0] * f0[0] + rel[1] * f0[1] + rel[2] * f0[2];
      proj.push([X, Y, Z]);
      minX = Math.min(minX, X);
      maxX = Math.max(maxX, X);
      minY = Math.min(minY, Y);
      maxY = Math.max(maxY, Y);
    }
    const sx = (minX + maxX) / 2;
    const sy = (minY + maxY) / 2;
    let d = 600;
    for (const [X, Y, Z] of proj) {
      d = Math.max(
        d,
        Math.abs(X - sx) / (k * TAN_FOV * aspect) - Z,
        Math.abs(Y - sy) / (k * TAN_FOV) - Z,
        -Z * 1.06, // 比包围盒平面更靠近相机一圈的光点也要留在相机前方
      );
    }
    fitCenter = [cx + r0[0] * sx + u0[0] * sy, cy + r0[1] * sx + u0[1] * sy, cz + r0[2] * sx + u0[2] * sy];
    fitAllDist = Math.max(600, d);
  }

  function focusStar(s, dist) {
    camT.tx = s.pos[0];
    camT.ty = s.pos[1];
    camT.tz = s.pos[2];
    camT.dist = dist || Math.max(180, (s.sigma || 60) * 2.2);
  }

  function resetView() {
    fitAllStars(); // 按当前窗口宽高重算：复位 = 重新装下所有光点
    camT.tx = fitCenter[0];
    camT.ty = fitCenter[1];
    camT.tz = fitCenter[2];
    camT.dist = fitAllDist;
    camT.az = DEFAULT_AZ;
    camT.el = DEFAULT_EL;
  }

  // —— 亮度：搜索命中全亮，其余压暗到几乎不可见；固有亮度 = 上下文长度因子 ——
  function applyBrightness() {
    if (!renderer) return;
    renderer.setBrightness((s) => {
      if (s.bg) return matchIds ? 0.25 : 0.6;
      const g = s.glow || 1;
      if (!matchIds) return g * (s.key === currentSessionId ? 1.1 : 1);
      return matchIds.has(s.key) ? 1.35 * g : 0.04;
    });
  }

  // —— 数据 ——
  function nodeById(id) {
    return stars.find((s) => s.id === id) || null;
  }

  async function load(force = false) {
    loading = true;
    errMsg = '';
    try {
      const r = await fetch(force ? '/api/memory/library/reindex' : '/api/memory/library', {
        method: force ? 'POST' : 'GET',
      });
      const d = await r.json();
      items = d.items || [];
      stats = d.stats || null;
      notice = d.notice || '';
      rebuildScene();
      applyBrightness();
      await tick();
      resize();
      if (!searched) {
        resetView();
        cam.dist = fitAllDist * 1.6; // 从更远处缓缓推入（进场动画），目标已含全部光点
      }
    } catch (e) {
      errMsg = `记忆库加载失败：${e}`;
    } finally {
      loading = false;
    }
  }

  const MODE_LABEL = (m, model) =>
    m === 'external' ? `向量检索 · ${model}` : m === 'local' ? '本地检索（未配置嵌入渠道）' : '关键词检索';

  async function doSearch() {
    const q = query.trim();
    if (!q) {
      clearSearch();
      return;
    }
    searching = true;
    searchNotice = '';
    try {
      const r = await fetch('/api/memory/search', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ query: q, top_k: 60 }),
      });
      const d = await r.json();
      results = d.results || [];
      searchNotice = d.notice || '';
      matchIds = new Set(results.map((x) => x.id));
      focusProj = null;
      searched = true;
      const label = MODE_LABEL(d.mode, d.model);
      searchInfo = `${results.length} 个光点 · ${label}`;
      if (d.mode === 'local') searchNotice = '设置 → 角色分工 → 向量嵌入，可开启真语义检索';
      const pts = results.map((x) => nodeById(x.id)).filter(Boolean);
      if (pts.length) fitStars(pts, 1.5);
      if (!results.length) searchInfo = `没有匹配「${q}」的会话 · ${label}`;
    } catch (e) {
      searchInfo = `检索失败：${e}`;
    } finally {
      searching = false;
      applyBrightness();
    }
  }

  async function doSimilar() {
    if (!selected) return;
    const src = selected;
    searching = true;
    searchNotice = '';
    query = '';
    focusProj = null;
    try {
      const r = await fetch('/api/memory/search', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ like: src.id, top_k: 24 }),
      });
      const d = await r.json();
      results = d.results || [];
      matchIds = new Set(results.map((x) => x.id));
      matchIds.add(src.id);
      searched = true;
      searchInfo = `与「${src.title}」相似的会话 · ${results.length} 个`;
      const pts = [...results.map((x) => nodeById(x.id)).filter(Boolean), src];
      if (pts.length > 1) fitStars(pts, 1.5);
      if (!results.length) searchInfo = `没有找到与「${src.title}」相近的会话`;
    } catch (e) {
      searchInfo = `相似检索失败：${e}`;
    } finally {
      searching = false;
      applyBrightness();
    }
  }

  function clearSearch() {
    query = '';
    searched = false;
    results = [];
    matchIds = null;
    focusProj = null;
    searchInfo = '';
    searchNotice = '';
    applyBrightness();
    resetView();
  }

  function focusProject(c) {
    const pts = stars.filter((s) => s.cluster === c.name);
    matchIds = new Set(pts.map((s) => s.id));
    focusProj = c.name;
    searched = false;
    results = [];
    searchInfo = `${c.name} · ${c.count} 个光点`;
    fitStars(pts, 1.6);
    applyBrightness();
  }

  /** 项目浮窗里点一行：再点当前筛选中的项目 = 取消筛选（回到全部） */
  function pickProject(c) {
    if (focusProj === c.name) clearSearch();
    else focusProject(c);
  }

  /** 左下角项目浮窗开合：与选中卡同占左下角，互斥显示 */
  function toggleProjPanel() {
    projPanelOpen = !projPanelOpen;
    if (projPanelOpen) selected = null;
  }

  function focusResult(hit) {
    const s = nodeById(hit.id);
    if (s) {
      focusStar(s);
      selected = s;
      projPanelOpen = false;
    }
  }

  function openNode(s) {
    dispatch('open', s.id);
  }

  // —— 指针交互 ——
  function pointerDown(e) {
    projPanelOpen = false; // 点星空即收起项目浮窗（点中恒星时选中卡也不会被浮窗压住）
    dragging = true;
    orbitDrag = e.shiftKey || e.button === 2;
    dragMoved = 0;
    lastX = e.clientX;
    lastY = e.clientY;
    uiCanvas.setPointerCapture?.(e.pointerId);
  }

  /** 拖动平移星云：目标向量限长在星场内（原按轴限幅，斜向组合会跑出星场，
   *  出现"有云无星"的空区——用户实拍） */
  function panBy(dx, dy) {
    const b = basis();
    const s = (2 * cam.dist * TAN_FOV) / H; // 世界单位 / 像素
    const d = [
      (-dx * b.right[0] + dy * b.up[0]) * s,
      (-dx * b.right[1] + dy * b.up[1]) * s,
      (-dx * b.right[2] + dy * b.up[2]) * s,
    ];
    const lim = sceneFieldR * 0.45;
    const nx = camT.tx + d[0];
    const ny = camT.ty + d[1];
    const nz = camT.tz + d[2];
    const len = Math.hypot(nx, ny, nz) || 1;
    const k = len > lim ? lim / len : 1;
    camT.tx = nx * k;
    camT.ty = ny * k;
    camT.tz = nz * k;
  }

  function pointerMove(e) {
    if (dragging) {
      const dx = e.clientX - lastX;
      const dy = e.clientY - lastY;
      lastX = e.clientX;
      lastY = e.clientY;
      dragMoved += Math.abs(dx) + Math.abs(dy);
      if (orbitDrag) {
        camT.az = cam.az - dx * 0.005;
        camT.el = Math.max(-1.35, Math.min(1.35, cam.el + dy * 0.004));
      } else {
        panBy(dx, dy);
      }
      return;
    }
    hoverId = hitTest(e);
  }

  function pointerUp(e) {
    if (!dragging) return;
    dragging = false;
    uiCanvas.releasePointerCapture?.(e.pointerId);
    if (dragMoved < 5) {
      const id = hitTest(e);
      const s = id ? nodeById(id) : null;
      selected = s;
      hoverId = id;
    }
  }

  function hitTest(e) {
    const rect = uiCanvas.getBoundingClientRect();
    const mx = e.clientX - rect.left;
    const my = e.clientY - rect.top;
    let best = null;
    let bestD = Infinity;
    for (const s of stars) {
      const p = screenPos.get(s.id);
      if (!p) continue;
      const d = Math.hypot(p.x - mx, p.y - my);
      if (d < 34 && d < bestD) {
        best = s;
        bestD = d;
      }
    }
    return best ? best.id : null;
  }

  function wheel(e) {
    e.preventDefault();
    const k = Math.exp(e.deltaY * 0.0012);
    // 拉远上限：默认全景（=进场视角，已是极限远距离）再放宽 35% 作余量，缩到最小仍是"一眼看全"
    camT.dist = Math.max(sceneRadius * 0.12, Math.min(fitAllDist * 1.35, camT.dist * k));
  }

  function onKey(e) {
    if (e.key === 'Escape') {
      if (projPanelOpen) projPanelOpen = false;
      else if (selected) selected = null;
      else if (searched) clearSearch();
      else dispatch('close');
    }
  }

  function relTime(iso) {
    if (!iso) return '';
    const t = Date.parse(iso);
    if (Number.isNaN(t)) return '';
    const m = Math.floor((Date.now() - t) / 60000);
    if (m < 1) return '刚刚';
    if (m < 60) return `${m} 分钟前`;
    const h = Math.floor(m / 60);
    if (h < 24) return `${h} 小时前`;
    const d = Math.floor(h / 24);
    if (d < 30) return `${d} 天前`;
    return new Date(t).toLocaleDateString();
  }

  $: modeLabel = stats
    ? (stats.mode === 'external' ? `嵌入 ${stats.model}` : '本地向量') +
      ` · 已索引 ${stats.indexed}/${stats.sessions}` +
      (stats.pending > 0 ? `（${stats.pending} 条待索引）` : '')
    : '';

  // —— 尺寸 / 覆盖层 / 渲染循环 ——
  function resize() {
    if (!hostEl) return;
    const r = hostEl.getBoundingClientRect();
    W = Math.max(320, r.width);
    H = Math.max(240, r.height);
    DPR = Math.min(2, window.devicePixelRatio || 1);
    if (uiCanvas) {
      uiCanvas.width = Math.round(W * DPR);
      uiCanvas.height = Math.round(H * DPR);
      uiCanvas.style.width = `${W}px`;
      uiCanvas.style.height = `${H}px`;
    }
    if (renderer) renderer.resize(W, H, DPR);
  }

  /** 2D 覆盖层：投影恒星、画选中环（只给手动选中那颗）、写标签 */
  function overlay(ctx, b) {
    ctx.setTransform(DPR, 0, 0, DPR, 0, 0);
    ctx.clearRect(0, 0, W, H);
    if (!stars.length) return;
    for (const s of stars) {
      const p = project(s.pos, b);
      if (p && p.x > -200 && p.x < W + 200 && p.y > -200 && p.y < H + 200) screenPos.set(s.id, p);
      else screenPos.delete(s.id);
    }
    const labels = [];
    ctx.textAlign = 'center';
    ctx.font = '12px "Segoe UI", "Microsoft YaHei", sans-serif';
    for (const s of stars) {
      const p = screenPos.get(s.id);
      if (!p) continue;
      const isMatch = matchIds && matchIds.has(s.id);
      const isHot = hoverId === s.id || selected?.id === s.id || s.id === currentSessionId;
      if (matchIds && !isMatch && !isHot) continue;
      // 圆环只属于"手动选中"的光点（单击恒星 / 点结果条目）。搜索命中、项目筛选、
      // 当前对话这些由筛选得到的高亮一律不套环（用户要求）——命中用亮度+结果面板表达
      if (selected?.id === s.id) {
        const rr = Math.max(7, Math.min(26, 1200 / Math.max(1, p.z / 40)));
        ctx.globalAlpha = 0.95;
        ctx.strokeStyle = `hsla(${s.hue},100%,86%,0.9)`;
        ctx.lineWidth = 1.8;
        ctx.beginPath();
        ctx.arc(p.x, p.y, rr, 0, Math.PI * 2);
        ctx.stroke();
      }
      if (isHot) labels.push([s, p]);
    }
    if (matchIds) {
      for (const r of results.slice(0, 8)) {
        const s = nodeById(r.id);
        const p = s && screenPos.get(s.id);
        if (s && p && !labels.find((l) => l[0].id === s.id)) labels.push([s, p]);
      }
    }
    for (const [s, p] of labels) {
      const text = s.title.length > 18 ? `${s.title.slice(0, 18)}…` : s.title;
      const w = ctx.measureText(text).width + 14;
      ctx.globalAlpha = 0.82;
      ctx.fillStyle = 'rgba(6,8,18,0.78)';
      ctx.beginPath();
      if (ctx.roundRect) ctx.roundRect(p.x - w / 2, p.y - 34, w, 19, 9);
      else ctx.rect(p.x - w / 2, p.y - 34, w, 19);
      ctx.fill();
      ctx.globalAlpha = 1;
      ctx.fillStyle = '#e8f1ff';
      ctx.fillText(text, p.x, p.y - 20);
    }
    ctx.globalAlpha = 1;
  }

  /** 没有 WebGL2 时的兜底：平面星图（同一套 3D 投影 + 2D 辉光） */
  function drawFallback2d(ctx, b, el) {
    ctx.setTransform(DPR, 0, 0, DPR, 0, 0);
    const g = ctx.createLinearGradient(0, 0, W * 0.4, H);
    g.addColorStop(0, '#05060f');
    g.addColorStop(0.6, '#070a18');
    g.addColorStop(1, '#04050c');
    ctx.fillStyle = g;
    ctx.fillRect(0, 0, W, H);
    ctx.globalCompositeOperation = 'lighter';
    for (const c of clusters) {
      const p = project(c.center, b);
      if (!p) continue;
      const R = Math.max(40, (c.sigma * 2.6 * (H / 2)) / (p.z * TAN_FOV));
      const rg = ctx.createRadialGradient(p.x, p.y, 0, p.x, p.y, R);
      rg.addColorStop(0, `hsla(${c.hue}, 90%, 62%, 0.28)`);
      rg.addColorStop(0.5, `hsla(${c.hue}, 88%, 55%, 0.10)`);
      rg.addColorStop(1, `hsla(${c.hue}, 85%, 48%, 0)`);
      ctx.fillStyle = rg;
      ctx.fillRect(p.x - R, p.y - R, R * 2, R * 2);
    }
    for (const s of stars) {
      const p = screenPos.get(s.id) || project(s.pos, b);
      if (!p) continue;
      const isMatch = !matchIds || matchIds.has(s.id);
      const a = (isMatch ? 0.85 : 0.06) * (0.75 + 0.25 * Math.sin(el * 0.001 + s.phase * 8));
      const R = 10 + s.size * 4;
      const rg = ctx.createRadialGradient(p.x, p.y, 0, p.x, p.y, R);
      rg.addColorStop(0, `hsla(${s.hue}, 100%, 92%, ${a})`);
      rg.addColorStop(0.35, `hsla(${s.hue}, 95%, 70%, ${a * 0.5})`);
      rg.addColorStop(1, `hsla(${s.hue}, 90%, 55%, 0)`);
      ctx.fillStyle = rg;
      ctx.fillRect(p.x - R, p.y - R, R * 2, R * 2);
    }
    ctx.globalCompositeOperation = 'source-over';
    overlay(ctx, b);
  }

  function draw(now) {
    raf = requestAnimationFrame(draw);
    if (!now) return;
    if (!t0) t0 = now;
    if (now - lastDraw < 1000 / 30) return; // 限帧 30fps
    lastDraw = now;
    const el = now - t0;
    cam.az += (camT.az - cam.az) * 0.09;
    cam.el += (camT.el - cam.el) * 0.09;
    cam.dist += (camT.dist - cam.dist) * 0.07;
    cam.tx += (camT.tx - cam.tx) * 0.07;
    cam.ty += (camT.ty - cam.ty) * 0.07;
    cam.tz += (camT.tz - cam.tz) * 0.07;
    const b = basis();
    if (renderer) {
      // 星云巨壳半径动态化：max(6×场景半径, 相机离中心距离×1.6)。
      // 平移+拉远的组合位置可能超过任何静态壳（用户实拍：退到最外围看到一颗
      // 贴图球悬在黑天里）——壳跟着相机长，相机永远在壳内，"球"在几何上不可能出现
      const camR = Math.hypot(b.pos[0], b.pos[1], b.pos[2]);
      renderer.render(
        { pos: b.pos, right: b.right, up: b.up, fwd: b.fwd, tanFov: TAN_FOV, center: [0, 0, 0], radius: Math.max(sceneRadius * 6.0, camR * 1.6) },
        { exposure: 1.55, time: el },
      );
    }
    const ctx = uiCanvas.getContext('2d');
    if (fallback2d) drawFallback2d(ctx, b, el);
    else {
      ctx.setTransform(DPR, 0, 0, DPR, 0, 0);
      ctx.clearRect(0, 0, W, H);
      overlay(ctx, b);
    }
  }

  onMount(() => {
    try {
      renderer = new NebulaRenderer(glCanvas);
      // 密度固定最低档，首次烘焙即生效；光点大小/亮度由上下文长度驱动（布局层给出）
      renderer.densityMul = NEBULA_DENSITY;
    } catch (e) {
      renderError = `${e.message || e}`;
      renderer = null;
      fallback2d = true;
    }
    resize();
    load();
    const onResize = () => resize();
    window.addEventListener('resize', onResize);
    raf = requestAnimationFrame(draw);
    return () => {
      window.removeEventListener('resize', onResize);
      cancelAnimationFrame(raf);
      if (renderer) renderer.dispose();
    };
  });

  onDestroy(() => cancelAnimationFrame(raf));
</script>

<svelte:window on:keydown={onKey} />

<div class="galaxy" bind:this={hostEl}>
  <canvas class="gx-gl" bind:this={glCanvas}></canvas>
  <canvas
    class="gx-ui"
    class:hover={!!hoverId}
    bind:this={uiCanvas}
    on:pointerdown={pointerDown}
    on:pointermove={pointerMove}
    on:pointerup={pointerUp}
    on:pointerleave={() => (hoverId = null)}
    on:contextmenu|preventDefault={() => {}}
    on:wheel={wheel}
    on:dblclick={resetView}
  ></canvas>

  <header class="gx-head">
    <div class="gx-title">
      <span class="gx-mark" aria-hidden="true"></span>
      <b>记忆库</b>
      <span class="gx-sub">{stats ? `${stats.sessions} 个会话 · ${stats.projects} 个项目` : ''}</span>
    </div>
    <div class="gx-search">
      <input
        bind:value={query}
        placeholder="搜一段记忆，如「登录 JWT」「星云配色」"
        on:keydown={(e) => e.key === 'Enter' && doSearch()}
        spellcheck="false"
      />
      <button class="gx-btn" on:click={doSearch} disabled={searching}>{searching ? '检索中…' : '搜索'}</button>
      {#if searched}
        <button class="gx-btn" on:click={clearSearch}>清除</button>
      {/if}
    </div>
    <div class="gx-tools">
      {#if modeLabel}<span class="gx-mode" title="向量索引状态">{modeLabel}</span>{/if}
      <button class="gx-btn" on:click={() => { reindexing = true; load(true).finally(() => (reindexing = false)); }} disabled={reindexing}>
        {reindexing ? '重建中…' : '重新索引'}
      </button>
      <button class="gx-btn ghost" on:click={() => dispatch('close')} title="关闭（Esc）">✕</button>
    </div>
  </header>

  {#if searchInfo || searchNotice}
    <div class="gx-status">{searchInfo}{#if searchNotice}<span class="gx-warn"> · {searchNotice}</span>{/if}</div>
  {/if}
  {#if notice}
    <div class="gx-status warn">{notice}</div>
  {/if}
  {#if fallback2d}
    <div class="gx-status warn">WebGL2 不可用（{renderError}），已切到平面星图</div>
  {/if}

  {#if loading}
    <div class="gx-center">正在点亮星云…</div>
  {:else if errMsg}
    <div class="gx-center err">{errMsg}</div>
  {:else if !items.length}
    <div class="gx-center">还没有会话记录，去聊一句，它就会成为星云里的一颗恒星</div>
  {/if}

  {#if searched}
    <aside class="gx-results">
      <div class="gx-results-head">
        检索结果 <b>{results.length}</b>
        <span class="flex-sp"></span>
        <button class="gx-mini" on:click={clearSearch}>显示全部</button>
      </div>
      <div class="gx-results-body">
        {#each results as r, i (r.id)}
          {@const n = nodeById(r.id)}
          <button class="gx-hit" class:active={selected?.id === r.id} on:click={() => focusResult(r)} on:dblclick={() => n && openNode(n)}>
            <span class="gx-rank">{i + 1}</span>
            <span class="gx-hit-main">
              <span class="gx-hit-title">{n ? n.title : r.id}</span>
              <span class="gx-hit-snip">{r.snippet}</span>
              <span class="gx-hit-meta">{n ? `${n.project} · ${relTime(n.updated_at)}` : ''}</span>
            </span>
            <span class="gx-score" title={`综合 ${r.score.toFixed(2)} · 向量 ${r.vector.toFixed(2)} · 词元 ${r.lexical.toFixed(2)}`}>{Math.round(r.score * 100)}</span>
          </button>
        {:else}
          <div class="gx-empty">没有匹配的会话</div>
        {/each}
      </div>
    </aside>
  {/if}

  {#if selected}
    <aside class="gx-card">
      <div class="gx-card-title">{selected.title}</div>
      <div class="gx-card-meta">
        {selected.project} · {relTime(selected.updated_at)} · {selected.count} 条消息{#if selected.archived} · 已归档{/if}
      </div>
      <div class="gx-card-snip">{selected.snippet || '（这条会话还没有摘要）'}</div>
      <div class="gx-card-actions">
        <button class="gx-btn" on:click={() => openNode(selected)}>打开会话</button>
        <button class="gx-btn" on:click={doSimilar} disabled={searching} title="用向量找和这条会话相近的记忆">相似会话</button>
        <button class="gx-btn" on:click={() => (selected = null)}>关闭</button>
      </div>
    </aside>
  {/if}

  {#if clusters.length}
    <div class="gx-projects">
      {#if projPanelOpen}
        <div class="gx-proj-panel">
          <div class="gx-proj-head">
            <span>项目列表</span>
            <span class="flex-sp"></span>
            {#if matchIds}
              <button class="gx-mini" on:click={clearSearch}>显示全部</button>
            {/if}
          </div>
          <div class="gx-proj-list">
            {#each clusters as c (c.name)}
              <button class="gx-proj-item" class:on={focusProj === c.name} on:click={() => pickProject(c)} title={`只看「${c.name}」的会话`}>
                <span class="gx-dot" style="background:hsl({c.hue}, 90%, 68%); box-shadow:0 0 8px hsl({c.hue}, 90%, 62%)"></span>
                <span class="gx-proj-name">{c.name}</span>
                <span class="gx-count">{c.count}</span>
              </button>
            {/each}
          </div>
        </div>
      {/if}
      <button class="gx-chip gx-proj-toggle" class:on={projPanelOpen || !!focusProj} on:click={toggleProjPanel} title="项目列表：点开按项目筛选光点">
        <svg class="gx-proj-ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"><path d="M20 19.5a1.8 1.8 0 0 0 1.8-1.8V8.3a1.8 1.8 0 0 0-1.8-1.8h-7.4a1.8 1.8 0 0 1-1.5-.8l-.8-1.2a1.8 1.8 0 0 0-1.5-.8H4a1.8 1.8 0 0 0-1.8 1.8v12.2A1.8 1.8 0 0 0 4 19.5Z"/></svg>
        <span class="gx-proj-label">{focusProj || '项目'}</span>
        <span class="gx-count">{clusters.length}</span>
        <svg class="gx-caret" class:open={projPanelOpen} viewBox="0 0 24 24"><path d="M6.5 9.2 12 14.7l5.5-5.5z" fill="currentColor"/></svg>
      </button>
    </div>
  {/if}

  <aside class="gx-params">
    <div class="gx-params-row">
      <span>集聚程度</span>
      <input type="range" min="0" max="1" step="0.05" bind:value={pCluster} on:input={applyCluster} title="最低=铺满画面；最高=集中在中央（最初的星团）" />
    </div>
  </aside>

  <div class="gx-hint">拖动平移星云 · Shift/右键拖动环绕 · 滚轮推拉 · 双击复位 · 单击恒星看详情</div>
</div>

<style>
  /* 星云视图恒为深空配色（不随主题切换） */
  .galaxy {
    position: fixed;
    inset: 0;
    z-index: 300;
    background: #04050c;
    overflow: hidden;
    user-select: none;
  }
  .gx-gl,
  .gx-ui {
    position: absolute;
    inset: 0;
    display: block;
  }
  .gx-ui {
    cursor: grab;
  }
  .gx-ui.hover {
    cursor: pointer;
  }
  .gx-ui:active {
    cursor: grabbing;
  }
  .flex-sp {
    flex: 1;
  }

  .gx-head {
    position: absolute;
    top: 0;
    left: 0;
    right: 0;
    display: flex;
    align-items: center;
    gap: 14px;
    padding: 12px 18px;
    background: linear-gradient(180deg, rgba(4, 6, 14, 0.92), rgba(4, 6, 14, 0.55) 70%, rgba(4, 6, 14, 0));
    backdrop-filter: blur(6px);
  }
  .gx-title {
    display: flex;
    align-items: center;
    gap: 9px;
    color: #eaf2ff;
    font-size: 15px;
    white-space: nowrap;
  }
  .gx-mark {
    width: 13px;
    height: 13px;
    border-radius: 50%;
    background: radial-gradient(circle at 35% 35%, #fff, #9fc8ff 35%, rgba(124, 196, 255, 0) 72%);
    box-shadow: 0 0 14px rgba(150, 200, 255, 0.9);
  }
  .gx-sub {
    color: rgba(190, 210, 240, 0.6);
    font-size: 12px;
  }
  .gx-search {
    display: flex;
    align-items: center;
    gap: 8px;
    flex: 1;
    max-width: 560px;
  }
  .gx-search input {
    flex: 1;
    background: rgba(255, 255, 255, 0.06);
    border: 1px solid rgba(140, 175, 230, 0.25);
    border-radius: 10px;
    color: #eaf2ff;
    font-size: 13px;
    padding: 8px 12px;
    outline: none;
    transition: border-color 0.15s, box-shadow 0.15s;
  }
  .gx-search input::placeholder {
    color: rgba(170, 190, 220, 0.45);
  }
  .gx-search input:focus {
    border-color: rgba(120, 200, 255, 0.7);
    box-shadow: 0 0 0 3px rgba(90, 170, 255, 0.14);
  }
  .gx-tools {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-left: auto;
  }
  .gx-mode {
    color: rgba(180, 205, 240, 0.6);
    font-size: 11.5px;
    white-space: nowrap;
  }
  .gx-btn {
    background: rgba(255, 255, 255, 0.07);
    border: 1px solid rgba(150, 180, 230, 0.28);
    color: #dfeaff;
    font-size: 12.5px;
    border-radius: 9px;
    padding: 7px 13px;
    cursor: pointer;
    transition: background 0.15s, border-color 0.15s, opacity 0.15s;
  }
  .gx-btn:hover:not(:disabled) {
    background: rgba(120, 170, 255, 0.16);
    border-color: rgba(150, 200, 255, 0.5);
  }
  .gx-btn:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .gx-btn.ghost {
    background: transparent;
    padding: 7px 10px;
  }
  .gx-btn.ghost:hover {
    background: rgba(255, 255, 255, 0.08);
  }

  .gx-status {
    position: absolute;
    top: 64px;
    left: 18px;
    color: rgba(200, 220, 250, 0.78);
    font-size: 12.5px;
    background: rgba(10, 16, 32, 0.6);
    border: 1px solid rgba(120, 160, 230, 0.18);
    border-radius: 9px;
    padding: 6px 11px;
    backdrop-filter: blur(4px);
  }
  .gx-status.warn {
    top: 100px;
    color: #ffd9a8;
    border-color: rgba(255, 190, 110, 0.32);
  }
  .gx-warn {
    color: #ffcf94;
  }

  .gx-center {
    position: absolute;
    left: 50%;
    top: 50%;
    transform: translate(-50%, -50%);
    color: rgba(200, 220, 250, 0.72);
    font-size: 14px;
    text-align: center;
    line-height: 1.9;
    max-width: 460px;
  }
  .gx-center.err {
    color: #ffb0b0;
  }

  .gx-results {
    position: absolute;
    top: 76px;
    right: 18px;
    width: min(360px, 34vw);
    max-height: min(64vh, 620px);
    display: flex;
    flex-direction: column;
    /* 透明质感（用户要求）：与项目 chips 同 token */
    background: rgba(255, 255, 255, 0.05);
    border: 1px solid rgba(140, 175, 225, 0.22);
    border-radius: 14px;
    backdrop-filter: blur(8px);
    overflow: hidden;
    box-shadow: 0 18px 50px rgba(0, 0, 0, 0.3);
  }
  .gx-results-head {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 11px 13px;
    color: #dfeaff;
    font-size: 12.5px;
    border-bottom: 1px solid rgba(120, 160, 230, 0.16);
  }
  .gx-results-head b {
    color: #8ec8ff;
  }
  .gx-mini {
    background: none;
    border: none;
    color: rgba(160, 195, 240, 0.8);
    font-size: 12px;
    cursor: pointer;
    padding: 2px 4px;
  }
  .gx-mini:hover {
    color: #cfe6ff;
    text-decoration: underline;
  }
  .gx-results-body {
    overflow: auto;
    padding: 6px;
  }
  .gx-empty {
    color: rgba(190, 210, 240, 0.6);
    font-size: 12.5px;
    padding: 16px 10px;
    text-align: center;
  }
  .gx-hit {
    display: flex;
    gap: 9px;
    width: 100%;
    text-align: left;
    background: transparent;
    border: 1px solid transparent;
    border-radius: 10px;
    padding: 8px 9px;
    cursor: pointer;
    color: inherit;
  }
  .gx-hit:hover,
  .gx-hit.active {
    background: rgba(90, 140, 230, 0.14);
    border-color: rgba(120, 170, 255, 0.3);
  }
  .gx-rank {
    flex: none;
    width: 18px;
    color: rgba(150, 185, 235, 0.75);
    font-size: 11.5px;
    padding-top: 2px;
  }
  .gx-hit-main {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .gx-hit-title {
    color: #eaf2ff;
    font-size: 13px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .gx-hit-snip {
    color: rgba(185, 205, 235, 0.72);
    font-size: 11.5px;
    line-height: 1.5;
    display: -webkit-box;
    -webkit-line-clamp: 2;
    -webkit-box-orient: vertical;
    overflow: hidden;
  }
  .gx-hit-meta {
    color: rgba(150, 175, 210, 0.6);
    font-size: 11px;
  }
  .gx-score {
    flex: none;
    color: #8ec8ff;
    font-size: 12px;
    font-weight: 600;
    padding-top: 1px;
  }

  .gx-card {
    position: absolute;
    left: 18px;
    bottom: 74px;
    width: min(340px, 32vw);
    /* 透明质感（用户要求）：与项目 chips 同 token */
    background: rgba(255, 255, 255, 0.05);
    border: 1px solid rgba(140, 175, 225, 0.22);
    border-radius: 14px;
    padding: 14px 15px;
    backdrop-filter: blur(8px);
    box-shadow: 0 18px 50px rgba(0, 0, 0, 0.3);
  }
  .gx-card-title {
    color: #f2f7ff;
    font-size: 14.5px;
    font-weight: 600;
    line-height: 1.5;
    margin-bottom: 5px;
  }
  .gx-card-meta {
    color: rgba(165, 190, 225, 0.72);
    font-size: 11.5px;
    margin-bottom: 9px;
  }
  .gx-card-snip {
    color: rgba(200, 218, 245, 0.85);
    font-size: 12.5px;
    line-height: 1.7;
    max-height: 132px;
    overflow: auto;
    margin-bottom: 12px;
  }
  .gx-card-actions {
    display: flex;
    gap: 8px;
  }

  /* 左下角项目：收起态只有一个按钮，点开才是项目列表浮窗（透明质感与结果面板/详情卡同 token） */
  .gx-projects {
    position: absolute;
    left: 18px;
    bottom: 18px;
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: 8px;
  }
  .gx-proj-panel {
    width: min(300px, 38vw);
    max-height: min(46vh, 420px);
    display: flex;
    flex-direction: column;
    background: rgba(255, 255, 255, 0.05);
    border: 1px solid rgba(140, 175, 225, 0.22);
    border-radius: 14px;
    backdrop-filter: blur(8px);
    overflow: hidden;
    box-shadow: 0 18px 50px rgba(0, 0, 0, 0.3);
  }
  .gx-proj-head {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 10px 12px;
    color: #dfeaff;
    font-size: 12.5px;
    border-bottom: 1px solid rgba(120, 160, 230, 0.16);
  }
  .gx-proj-list {
    display: flex;
    flex-direction: column;
    gap: 2px;
    overflow: auto;
    padding: 6px;
  }
  .gx-proj-item {
    display: flex;
    align-items: center;
    gap: 8px;
    width: 100%;
    text-align: left;
    background: transparent;
    border: 1px solid transparent;
    border-radius: 10px;
    padding: 7px 9px;
    color: rgba(215, 230, 250, 0.9);
    font-size: 12.5px;
    cursor: pointer;
    transition: background 0.15s, border-color 0.15s;
  }
  .gx-proj-item:hover,
  .gx-proj-item.on {
    background: rgba(110, 160, 255, 0.16);
    border-color: rgba(140, 190, 255, 0.4);
    color: #f0f6ff;
  }
  .gx-proj-name {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .gx-proj-toggle {
    max-width: min(300px, 38vw);
  }
  .gx-proj-label {
    max-width: 168px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .gx-proj-ico {
    width: 13px;
    height: 13px;
    flex: none;
    opacity: 0.85;
  }
  .gx-caret {
    width: 12px;
    height: 12px;
    flex: none;
    opacity: 0.7;
    transform: rotate(180deg); /* 收起：浮窗向上开，箭头朝上 */
    transition: transform 0.18s;
  }
  .gx-caret.open {
    transform: rotate(0deg);
  }
  .gx-chip {
    display: inline-flex;
    align-items: center;
    gap: 7px;
    background: rgba(255, 255, 255, 0.05);
    border: 1px solid rgba(140, 175, 225, 0.22);
    color: rgba(215, 230, 250, 0.85);
    font-size: 12px;
    border-radius: 999px;
    padding: 5px 11px;
    cursor: pointer;
    transition: background 0.15s, border-color 0.15s;
  }
  .gx-chip:hover,
  .gx-chip.on {
    background: rgba(110, 160, 255, 0.16);
    border-color: rgba(140, 190, 255, 0.48);
    color: #eaf3ff;
  }
  .gx-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    flex: none;
  }
  .gx-count {
    color: rgba(160, 190, 230, 0.65);
    font-size: 11px;
  }
  .gx-hint {
    position: absolute;
    right: 18px;
    bottom: 18px;
    color: rgba(150, 175, 210, 0.5);
    font-size: 11.5px;
  }
  /* 无卡片底：滑条直接浮在星空上（用户要求），加轻微文字投影保证可读 */
  .gx-params {
    position: absolute;
    right: 18px;
    bottom: 44px;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .gx-params-row {
    display: flex;
    align-items: center;
    gap: 10px;
    color: rgba(190, 210, 240, 0.8);
    font-size: 11.5px;
    white-space: nowrap;
    text-shadow: 0 1px 5px rgba(2, 4, 10, 0.9);
  }
  .gx-params-row span {
    width: 56px;
    flex: none;
  }
  .gx-params-row input[type='range'] {
    width: 128px;
    accent-color: #7db8ff;
    cursor: pointer;
  }
</style>
