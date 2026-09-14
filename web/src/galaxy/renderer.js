// 记忆库星云的 WebGL2 渲染器（P14 八轮后改为烘焙架构，借鉴 wwwtyro/space-3d，Unlicense）：
//   · 打开视图时一次性把星云烘进 3×2 cubemap 图集（位移迭代噪声 + 经典 4D Perlin，
//     青蓝气×3 / 暗尘带×1 / 橙锈纱×2 分层 alpha 叠加）——离线烘焙的细节密度
//     远高于每帧 44 步光线步进
//   · 运行时：pass1 视线与巨壳求交采样图集 + 项目团辉光；pass2 上采样分级；
//     pass3 恒星点精灵（加性混合）
// 帧时自适应分辨率保留：卡了就降采样，闲了再升回来。

import { NEBULA_VS, SKY_FS, BAKE_FS, COMPOSITE_FS, STAR_VS, STAR_FS } from './shaders.js';

const FLOATS_PER_STAR = 8; // pos3 + size + color3 + spike

// 烘焙分层（调色对齐哈勃参考图）：青蓝气为主体，暗尘带压黑，橙锈纱为强配角。
// 位移迭代噪声输出集中在 0.5 附近（方差小），intensity 放大 + falloff 切出云块/暗隙。
// scale 决定形态占比：~1.0 量级时一块云约占天球 1/5（参考图的大形态尺度）
// 烘焙分层（调色对齐哈勃参考图）：绘制顺序 = 蓝气 → 橙纱 → 暗尘带压轴。
// 暗尘带最后画：在蓝、橙上都刻出近黑的暗带——参考图近一半面积是黑场，
// 没有这层压轴，蓝+橙的覆盖并集会填满整屏（无黑场=不像照片）。
// P14 十一轮"雾太浓"：falloff 全面抬高压低覆盖——参考图的云是透的、
// 黑场占一半，云是亮的结构不是整片底
// P14 十二轮"稀疏≠碎布片"：云要保持连体（低频场连续），稀薄靠
// ①平滑渐变（falloff 2.6~5，不用近二值阈值）②低 opacity 上限（0.3~0.5，
// 大部分区域半透明，只有核心稍厚）③轻微高频调制。黑场由暗尘带压轴刻出。
const BAKE_LAYERS = [
  // 深蓝底（连体薄纱的远景层）
  { color: [0.07, 0.26, 0.62], scale: 2.60, intensity: 1.4, falloff: 2.6, opacity: 0.40, offset: [37.0, 11.0, 71.0] },
  // 青蓝主气
  { color: [0.18, 0.62, 1.00], scale: 1.70, intensity: 1.6, falloff: 2.8, opacity: 0.55, offset: [-51.0, 87.0, 23.0] },
  { color: [0.28, 0.72, 1.00], scale: 1.30, intensity: 1.5, falloff: 3.0, opacity: 0.52, offset: [123.0, -44.0, 9.0] },
  // 橙锈纱（连体的锈色纱带）
  { color: [0.72, 0.30, 0.08], scale: 2.30, intensity: 1.6, falloff: 4.2, opacity: 0.48, offset: [-77.0, 29.0, 155.0] },
  { color: [0.55, 0.22, 0.06], scale: 1.60, intensity: 1.7, falloff: 4.8, opacity: 0.42, offset: [8.0, -99.0, 47.0] },
  // 暗尘带（压轴：在蓝、橙上刻近黑的暗带与暗腔）
  { color: [0.015, 0.008, 0.02], scale: 1.60, intensity: 1.7, falloff: 3.6, opacity: 0.60, offset: [91.0, 140.0, -66.0] },
];

export class NebulaRenderer {
  constructor(canvas) {
    this.canvas = canvas;
    this.gl = canvas.getContext('webgl2', {
      antialias: false,
      alpha: false,
      depth: false,
      // 保留绘制缓冲：方便外部（截图/调试）在任意时刻读到上一帧内容
      preserveDrawingBuffer: true,
      powerPreference: 'high-performance',
    });
    if (!this.gl) throw new Error('WebGL2 不可用');
    this.gl_ = this.gl;
    this.stars = [];
    this.starCount = 0;
    this.quality = 0.55; // 背景采样的分辨率比例
    this.acc = 0;
    this.frames = 0;
    this.lastTime = 0;
    this.goodStreak = 0;
    this.w = 1;
    this.h = 1;
    this.dpr = 1;
    this._baked = false;
    this.bakeSize = 768; // cubemap 每面分辨率（图集 3×2 面）
    this.setup();
  }

  static available() {
    try {
      const c = document.createElement('canvas');
      return !!c.getContext('webgl2');
    } catch {
      return false;
    }
  }

  compile(type, src) {
    const gl = this.gl_;
    const sh = gl.createShader(type);
    gl.shaderSource(sh, src);
    gl.compileShader(sh);
    if (!gl.getShaderParameter(sh, gl.COMPILE_STATUS)) {
      const log = gl.getShaderInfoLog(sh);
      gl.deleteShader(sh);
      throw new Error(`着色器编译失败: ${log}`);
    }
    return sh;
  }

  program(vsSrc, fsSrc) {
    const gl = this.gl_;
    const p = gl.createProgram();
    const vs = this.compile(gl.VERTEX_SHADER, vsSrc);
    const fs = this.compile(gl.FRAGMENT_SHADER, fsSrc);
    gl.attachShader(p, vs);
    gl.attachShader(p, fs);
    gl.linkProgram(p);
    gl.deleteShader(vs);
    gl.deleteShader(fs);
    if (!gl.getProgramParameter(p, gl.LINK_STATUS)) {
      throw new Error(`程序链接失败: ${gl.getProgramInfoLog(p)}`);
    }
    return p;
  }

  setup() {
    const gl = this.gl_;
    this.progSky = this.program(NEBULA_VS, SKY_FS);
    this.progBake = this.program(NEBULA_VS, BAKE_FS);
    this.progComp = this.program(NEBULA_VS, COMPOSITE_FS);
    this.progStar = this.program(STAR_VS, STAR_FS);
    this.uni = (p, names) => {
      const o = {};
      for (const n of names) o[n] = gl.getUniformLocation(p, n);
      return o;
    };
    this.uSky = this.uni(this.progSky, [
      'uRes', 'uCam', 'uRight', 'uUp', 'uFwd', 'uTanFov', 'uCenter', 'uShellR',
      'uAtlas', 'uCPos', 'uCHue', 'uCWgt', 'uCCount',
    ]);
    this.uBake = this.uni(this.progBake, [
      'uFaceFwd', 'uFaceRight', 'uFaceUp', 'uFaceSize', 'uViewportOrigin',
      'uColor', 'uOffset', 'uScale', 'uIntensity', 'uFalloff', 'uOpacity', 'uDensityMul',
    ]);
    this.uComp = this.uni(this.progComp, ['uTex', 'uRes', 'uExposure', 'uZoom', 'uRd', 'uTime']);
    this.uStar = this.uni(this.progStar, ['uRight', 'uUp', 'uFwd', 'uCam', 'uRes', 'uTanFov', 'uTime', 'uSizeMul', 'uGlow']);
    // 参数滑条状态（Galaxy.svelte 通过 setter 更新）
    this.starSizeMul = 1.0;
    this.starGlow = 1.0;
    this.densityMul = 1.0;
    this.emptyVao = gl.createVertexArray();
    // 烘焙图集：3×2 个立方体贴图面
    this.atlasTex = gl.createTexture();
    gl.bindTexture(gl.TEXTURE_2D, this.atlasTex);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA8, this.bakeSize * 3, this.bakeSize * 2, 0, gl.RGBA, gl.UNSIGNED_BYTE, null);
    this.atlasFbo = gl.createFramebuffer();
    gl.bindFramebuffer(gl.FRAMEBUFFER, this.atlasFbo);
    gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0, gl.TEXTURE_2D, this.atlasTex, 0);
    // 背景半分辨率 FBO
    this.fbo = gl.createFramebuffer();
    this.fboTex = gl.createTexture();
    gl.bindTexture(gl.TEXTURE_2D, this.fboTex);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
    gl.bindFramebuffer(gl.FRAMEBUFFER, this.fbo);
    gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0, gl.TEXTURE_2D, this.fboTex, 0);
    gl.bindFramebuffer(gl.FRAMEBUFFER, null);
    gl.disable(gl.DEPTH_TEST);
    gl.disable(gl.CULL_FACE);
    // 星点缓冲
    this.starBuf = gl.createBuffer();
    this.brightBuf = gl.createBuffer();
    this.starVao = gl.createVertexArray();
    gl.bindVertexArray(this.starVao);
    gl.bindBuffer(gl.ARRAY_BUFFER, this.starBuf);
    const stride = FLOATS_PER_STAR * 4;
    gl.enableVertexAttribArray(0);
    gl.vertexAttribPointer(0, 3, gl.FLOAT, false, stride, 0);
    gl.enableVertexAttribArray(1);
    gl.vertexAttribPointer(1, 1, gl.FLOAT, false, stride, 12);
    gl.enableVertexAttribArray(2);
    gl.vertexAttribPointer(2, 3, gl.FLOAT, false, stride, 16);
    gl.enableVertexAttribArray(4);
    gl.vertexAttribPointer(4, 1, gl.FLOAT, false, stride, 28);
    gl.bindBuffer(gl.ARRAY_BUFFER, this.brightBuf);
    gl.enableVertexAttribArray(3);
    gl.vertexAttribPointer(3, 1, gl.FLOAT, false, 0, 0);
    gl.bindVertexArray(null);
  }

  /** 一次性把星云烘进图集：6 个立方体面 × 分层 alpha 叠加 */
  bakeBackdrop() {
    const gl = this.gl_;
    const S = this.bakeSize;
    const faces = [
      { fwd: [1, 0, 0], right: [0, 0, -1], up: [0, 1, 0] },
      { fwd: [-1, 0, 0], right: [0, 0, 1], up: [0, 1, 0] },
      { fwd: [0, 1, 0], right: [1, 0, 0], up: [0, 0, 1] },
      { fwd: [0, -1, 0], right: [1, 0, 0], up: [0, 0, -1] },
      { fwd: [0, 0, 1], right: [1, 0, 0], up: [0, 1, 0] },
      { fwd: [0, 0, -1], right: [-1, 0, 0], up: [0, 1, 0] },
    ];
    gl.bindFramebuffer(gl.FRAMEBUFFER, this.atlasFbo);
    gl.viewport(0, 0, S * 3, S * 2);
    gl.clearColor(0, 0, 0, 1);
    gl.clear(gl.COLOR_BUFFER_BIT);
    gl.enable(gl.BLEND);
    gl.blendFunc(gl.SRC_ALPHA, gl.ONE_MINUS_SRC_ALPHA);
    gl.useProgram(this.progBake);
    gl.uniform2f(this.uBake.uFaceSize, S, S);
    gl.uniform1f(this.uBake.uDensityMul, this.densityMul);
    gl.bindVertexArray(this.emptyVao);
    faces.forEach((f, i) => {
      const ox = (i % 3) * S;
      const oy = Math.floor(i / 3) * S;
      gl.viewport(ox, oy, S, S);
      gl.uniform2f(this.uBake.uViewportOrigin, ox, oy);
      gl.uniform3fv(this.uBake.uFaceFwd, f.fwd);
      gl.uniform3fv(this.uBake.uFaceRight, f.right);
      gl.uniform3fv(this.uBake.uFaceUp, f.up);
      BAKE_LAYERS.forEach((L, li) => {
        // 低密度时保住暗尘带：黑场是构图的一半，跟着密度一起消失会只剩奶雾
        const isDark = li === BAKE_LAYERS.length - 1;
        gl.uniform1f(this.uBake.uDensityMul, isDark ? Math.max(0.72, this.densityMul) : this.densityMul);
        gl.uniform3fv(this.uBake.uColor, L.color);
        gl.uniform3fv(this.uBake.uOffset, L.offset);
        gl.uniform1f(this.uBake.uScale, L.scale);
        gl.uniform1f(this.uBake.uIntensity, L.intensity);
        gl.uniform1f(this.uBake.uFalloff, L.falloff);
        gl.uniform1f(this.uBake.uOpacity, L.opacity);
        gl.drawArrays(gl.TRIANGLES, 0, 3);
      });
    });
    gl.disable(gl.BLEND);
    gl.bindFramebuffer(gl.FRAMEBUFFER, null);
    this._baked = true;
  }

  /** 参数滑条：光点大小（uniform 实时生效） */
  setStarSizeMul(m) {
    this.starSizeMul = m;
  }

  /** 参数滑条：光点发光程度（uniform 实时生效） */
  setStarGlow(g) {
    this.starGlow = g;
  }

  /** 参数滑条：星云密度——需要重烘焙；拖动时节流（≥350ms 一次），停止后补一次终值 */
  setDensityMul(m) {
    this.densityMul = m;
    const now = performance.now();
    clearTimeout(this._bakeTimer);
    if (now - (this._lastBakeAt || 0) > 350) {
      this._lastBakeAt = now;
      this.bakeBackdrop();
    } else {
      this._bakeTimer = setTimeout(() => {
        this._lastBakeAt = performance.now();
        this.bakeBackdrop();
      }, 350);
    }
  }

  /**
   * stars: [{ pos:[x,y,z], size, color:[r,g,b], spike:0|1, bright, key? }]
   * 一次性重建缓冲（会话星 + 背景星都在这里）。
   */
  setStars(stars) {
    const gl = this.gl_;
    this.stars = stars;
    this.keyIndex = new Map();
    const arr = new Float32Array(stars.length * FLOATS_PER_STAR);
    const br = new Float32Array(stars.length);
    stars.forEach((s, i) => {
      const o = i * FLOATS_PER_STAR;
      arr[o] = s.pos[0];
      arr[o + 1] = s.pos[1];
      arr[o + 2] = s.pos[2];
      arr[o + 3] = s.size;
      arr[o + 4] = s.color[0];
      arr[o + 5] = s.color[1];
      arr[o + 6] = s.color[2];
      arr[o + 7] = s.spike ? 1 : 0;
      br[i] = s.bright ?? 1;
      if (s.key) this.keyIndex.set(s.key, i);
    });
    gl.bindBuffer(gl.ARRAY_BUFFER, this.starBuf);
    gl.bufferData(gl.ARRAY_BUFFER, arr, gl.STATIC_DRAW);
    gl.bindBuffer(gl.ARRAY_BUFFER, this.brightBuf);
    gl.bufferData(gl.ARRAY_BUFFER, br, gl.DYNAMIC_DRAW);
    this.starCount = stars.length;
  }

  /** 只更新亮度（搜索命中的高亮/暗淡），按 key 定位 */
  setBrightness(fn) {
    const gl = this.gl_;
    if (!this.starCount) return;
    const br = new Float32Array(this.starCount);
    const prev = this._bright || new Float32Array(this.starCount);
    for (let i = 0; i < this.starCount; i++) {
      const s = this.stars[i];
      br[i] = fn(s, i);
      prev[i] = br[i];
    }
    this._bright = prev;
    gl.bindBuffer(gl.ARRAY_BUFFER, this.brightBuf);
    gl.bufferSubData(gl.ARRAY_BUFFER, 0, br);
  }

  /**
   * 星云团（每个项目一团）：pos 世界坐标、radius 包络半径、weight 0..1
   * （会话越多越重）、hue 为该团真实色相（度）。最多 12 团。
   */
  setClusters(list) {
    const n = Math.min(12, list.length);
    this.cCount = n;
    this.cPos = new Float32Array(36);
    this.cRad = new Float32Array(12);
    this.cWgt = new Float32Array(12);
    this.cHue = new Float32Array(12);
    for (let i = 0; i < n; i++) {
      const c = list[i];
      this.cPos.set(c.pos, i * 3);
      this.cRad[i] = c.radius;
      this.cWgt[i] = c.weight;
      this.cHue[i] = c.hue || 0;
    }
  }

  resize(w, h, dpr) {
    const gl = this.gl_;
    this.w = w;
    this.h = h;
    this.dpr = Math.min(dpr || 1, 1.75);
    this.canvas.width = Math.round(w * this.dpr);
    this.canvas.height = Math.round(h * this.dpr);
    this.canvas.style.width = `${w}px`;
    this.canvas.style.height = `${h}px`;
    const rw = Math.max(2, Math.round(this.canvas.width * this.quality));
    const rh = Math.max(2, Math.round(this.canvas.height * this.quality));
    gl.bindTexture(gl.TEXTURE_2D, this.fboTex);
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA8, rw, rh, 0, gl.RGBA, gl.UNSIGNED_BYTE, null);
  }

  /** 帧时自适应：连续几帧偏慢就降采样，明显富余再升回来 */
  adapt(frameMs) {
    this.acc += frameMs;
    this.frames++;
    if (this.frames < 24) return;
    const avg = this.acc / this.frames;
    this.acc = 0;
    this.frames = 0;
    const before = this.quality;
    if (avg > 26 && this.quality > 0.42) this.quality = Math.max(0.42, this.quality - 0.08);
    else if (avg < 13 && this.quality < 0.75) {
      this.goodStreak++;
      if (this.goodStreak >= 3) {
        this.quality = Math.min(0.75, this.quality + 0.05);
        this.goodStreak = 0;
      }
    } else this.goodStreak = 0;
    if (before !== this.quality) this.resize(this.w, this.h, this.dpr);
  }

  /**
   * cam: { pos:[x,y,z], right, up, fwd, tanFov, center:[x,y,z], radius }
   * radius = 星云巨壳半径（背景采样用）。opts: { exposure, time }
   */
  render(cam, opts) {
    const gl = this.gl_;
    const t0 = performance.now();
    if (!this._baked) this.bakeBackdrop();
    const rw = Math.max(2, Math.round(this.canvas.width * this.quality));
    const rh = Math.max(2, Math.round(this.canvas.height * this.quality));
    // pass1 烘焙星云壳（方向采样 + 团辉光）
    gl.bindFramebuffer(gl.FRAMEBUFFER, this.fbo);
    gl.viewport(0, 0, rw, rh);
    gl.useProgram(this.progSky);
    gl.uniform2f(this.uSky.uRes, rw, rh);
    gl.uniform3fv(this.uSky.uCam, cam.pos);
    gl.uniform3fv(this.uSky.uRight, cam.right);
    gl.uniform3fv(this.uSky.uUp, cam.up);
    gl.uniform3fv(this.uSky.uFwd, cam.fwd);
    gl.uniform1f(this.uSky.uTanFov, cam.tanFov);
    gl.uniform3fv(this.uSky.uCenter, cam.center);
    gl.uniform1f(this.uSky.uShellR, cam.radius);
    gl.uniform3fv(this.uSky.uCPos, this.cPos || new Float32Array(36));
    gl.uniform1fv(this.uSky.uCHue, this.cHue || new Float32Array(12));
    gl.uniform1fv(this.uSky.uCWgt, this.cWgt || new Float32Array(12));
    gl.uniform1i(this.uSky.uCCount, this.cCount || 0);
    gl.activeTexture(gl.TEXTURE0);
    gl.bindTexture(gl.TEXTURE_2D, this.atlasTex);
    gl.uniform1i(this.uSky.uAtlas, 0);
    gl.bindVertexArray(this.emptyVao);
    gl.drawArrays(gl.TRIANGLES, 0, 3);
    // pass2 上采样 + 色调映射
    gl.bindFramebuffer(gl.FRAMEBUFFER, null);
    gl.viewport(0, 0, this.canvas.width, this.canvas.height);
    gl.useProgram(this.progComp);
    gl.activeTexture(gl.TEXTURE0);
    gl.bindTexture(gl.TEXTURE_2D, this.fboTex);
    gl.uniform1i(this.uComp.uTex, 0);
    gl.uniform2f(this.uComp.uRes, this.canvas.width, this.canvas.height);
    gl.uniform1f(this.uComp.uExposure, opts.exposure);
    const distC = Math.hypot(cam.pos[0] - cam.center[0], cam.pos[1] - cam.center[1], cam.pos[2] - cam.center[2]) || 1.0;
    gl.uniform1f(this.uComp.uZoom, cam.radius / distC);
    gl.uniform3fv(this.uComp.uRd, cam.fwd);
    gl.uniform1f(this.uComp.uTime, opts.time || 0);
    gl.drawArrays(gl.TRIANGLES, 0, 3);
    // pass3 恒星（加性）
    this.drawStars(cam, opts);
    this.adapt(performance.now() - t0);
  }

  drawStars(cam, opts) {
    const gl = this.gl_;
    if (!this.starCount) return;
    gl.enable(gl.BLEND);
    gl.blendFunc(gl.SRC_ALPHA, gl.ONE);
    gl.useProgram(this.progStar);
    gl.uniform3fv(this.uStar.uRight, cam.right);
    gl.uniform3fv(this.uStar.uUp, cam.up);
    gl.uniform3fv(this.uStar.uFwd, cam.fwd);
    gl.uniform3fv(this.uStar.uCam, cam.pos);
    gl.uniform2f(this.uStar.uRes, this.canvas.width, this.canvas.height);
    gl.uniform1f(this.uStar.uTanFov, cam.tanFov);
    gl.uniform1f(this.uStar.uTime, opts.time || 0);
    gl.uniform1f(this.uStar.uSizeMul, this.starSizeMul);
    gl.uniform1f(this.uStar.uGlow, this.starGlow);
    gl.bindVertexArray(this.starVao);
    gl.drawArrays(gl.POINTS, 0, this.starCount);
    gl.disable(gl.BLEND);
    gl.bindVertexArray(null);
  }

  dispose() {
    const gl = this.gl_;
    if (!gl) return;
    for (const p of [this.progSky, this.progBake, this.progComp, this.progStar]) if (p) gl.deleteProgram(p);
    for (const b of [this.starBuf, this.brightBuf]) if (b) gl.deleteBuffer(b);
    for (const t of [this.atlasTex, this.fboTex]) if (t) gl.deleteTexture(t);
    for (const f of [this.atlasFbo, this.fbo]) if (f) gl.deleteFramebuffer(f);
    if (this.starVao) gl.deleteVertexArray(this.starVao);
    if (this.emptyVao) gl.deleteVertexArray(this.emptyVao);
  }
}
