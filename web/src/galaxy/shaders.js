// 记忆库星云的 GLSL。八轮后改为「离线烘焙」架构（借鉴 wwwtyro/space-3d，Unlicense）：
//   · BAKE_FS：打开视图时把星云一次性烘进 3×2 cubemap 图集——位移迭代噪声
//     （6 步域位移，经典 4D Perlin，Ashima MIT）产生平滑无线的密度场，
//     多层（青蓝气×3 / 暗尘带×1 / 橙锈纱×2）按 alpha 叠出照片级云雾
//   · SKY_FS：运行时把视线与巨壳求交采样图集（从每帧 44 步光线步进降为 1 次采样），
//     再按项目团方向叠同色辉光——星云与数据仍一一对应
//   · COMPOSITE_FS：ACES + 冷色深化 + 轻对比 + 暗角 + 抖动（无任何描线）
//   · 恒星 = 加性点精灵（用户认可的光点效果）
// 历史教训见 docs/ROADMAP.md 第 16 条：ridged+阈值=描线（用户否）、纯低频=失焦（用户否）。

import { CNOISE_4D } from './classic-noise-4d.js';

export const NEBULA_VS = `#version 300 es
precision highp float;
const vec2 P[3] = vec2[3](vec2(-1.0,-1.0), vec2(3.0,-1.0), vec2(-1.0,3.0));
void main() { gl_Position = vec4(P[gl_VertexID], 0.0, 1.0); }
`;

// 烘焙：单层星云。位移迭代噪声（space-3d 的 nebula()）生成密度，
// 输出 = 指定颜色 ×（密度×强度）^衰减 的 alpha 层，由渲染端按 over 混合叠层。
// 注意 gl_FragCoord 是窗口坐标不是视口坐标——图集分面渲染必须传视口原点校正
export const BAKE_FS = `#version 300 es
precision highp float;
out vec4 outColor;
uniform vec3 uFaceFwd;
uniform vec3 uFaceRight;
uniform vec3 uFaceUp;
uniform vec2 uFaceSize;
uniform vec2 uViewportOrigin;
uniform vec3 uColor;
uniform vec3 uOffset;
uniform float uScale;
uniform float uIntensity;
uniform float uFalloff;
uniform float uOpacity;   // 不透明度上限：云最厚处也只有这么"浓"（薄纱感的关键）
uniform float uDensityMul; // 星云密度滑条：整体 alpha 乘数（0.35~1.6）
${CNOISE_4D}
float noise(vec3 p) {
  return 0.5 * cnoise(vec4(p, 0.0)) + 0.5;
}
float nebula(vec3 p) {
  const int STEPS = 6;
  float scale = pow(2.0, float(STEPS));
  vec3 displace = vec3(0.0);
  for (int i = 0; i < STEPS; i++) {
    displace = vec3(
      noise(p.xyz * scale + displace),
      noise(p.yzx * scale + displace),
      noise(p.zxy * scale + displace)
    );
    scale *= 0.5;
  }
  return noise(p * scale + displace);
}
void main() {
  vec2 local = gl_FragCoord.xy - uViewportOrigin;
  vec2 ndc = (local - 0.5 * uFaceSize) / (0.5 * uFaceSize.y);
  vec3 dir = normalize(uFaceFwd + uFaceRight * ndc.x + uFaceUp * ndc.y);
  float d = nebula(dir * uScale + uOffset);
  // 轻微高频调制（只打碎均匀感，保持云连体——重侵蚀会把云切成孤岛碎布片）
  d *= 0.72 + 0.28 * noise(dir * uScale * 3.0 + uOffset * 0.7 + 5.0);
  // 平滑渐变（不切二值）：大部分区域半透明，核心稍厚，边缘渐隐——靠 uOpacity
  // 压低上限实现"稀薄"，而不是靠硬阈值挖洞
  float a = min(1.0, pow(min(1.0, d * uIntensity), uFalloff) * uOpacity * uDensityMul);
  outColor = vec4(uColor, a);
}
`;

// 运行时背景：视线与以 uCenter 为球心的巨壳求交，按命中方向采样烘焙图集；
// 再按各项目团方向叠同色高斯辉光（云与数据一一对应）
export const SKY_FS = `#version 300 es
precision highp float;
out vec4 outColor;
uniform vec2 uRes;
uniform vec3 uCam;
uniform vec3 uRight;
uniform vec3 uUp;
uniform vec3 uFwd;
uniform float uTanFov;
uniform vec3 uCenter;
uniform float uShellR;
uniform highp sampler2D uAtlas;
uniform vec3 uCPos[12];
uniform float uCHue[12];
uniform float uCWgt[12];
uniform int uCCount;

vec3 hue2rgb(float h) {
  h = fract(h / 360.0) * 6.0;
  return clamp(vec3(abs(h - 3.0) - 1.0, 2.0 - abs(h - 2.0), 2.0 - abs(h - 4.0)), 0.0, 1.0);
}

// 方向 → 图集 UV（面基与烘焙端 BAKE_FS 严格互逆）
vec2 atlasUV(vec3 d) {
  vec3 a = abs(d);
  vec2 uv;
  float tile;
  if (a.x >= a.y && a.x >= a.z) {
    if (d.x > 0.0) { uv = vec2(-d.z, d.y) / a.x; tile = 0.0; }
    else           { uv = vec2(d.z, d.y) / a.x; tile = 1.0; }
  } else if (a.y >= a.z) {
    if (d.y > 0.0) { uv = vec2(d.x, d.z) / a.y; tile = 2.0; }
    else           { uv = vec2(d.x, -d.z) / a.y; tile = 3.0; }
  } else {
    if (d.z > 0.0) { uv = vec2(d.x, d.y) / a.z; tile = 4.0; }
    else           { uv = vec2(-d.x, d.y) / a.z; tile = 5.0; }
  }
  float tx = mod(tile, 3.0);
  float ty = floor(tile / 3.0);
  return (vec2(tx, ty) + uv * 0.5 + 0.5) / vec2(3.0, 2.0);
}

void main() {
  vec2 uv = (gl_FragCoord.xy - 0.5 * uRes) / uRes.y;
  vec3 rd = normalize(uRight * (uv.x * 2.0 * uTanFov) + uUp * (uv.y * 2.0 * uTanFov) + uFwd);

  vec3 oc = uCam - uCenter;
  float b = dot(oc, rd);
  float c = dot(oc, oc) - uShellR * uShellR;
  float h = b * b - c;
  vec3 dir;
  if (h > 0.0) {
    float t = (c > 0.0) ? (-b - sqrt(h)) : (-b + sqrt(h)); // 壳外取近根，壳内取远根
    dir = normalize(oc + rd * t);
  } else {
    dir = rd; // 壳外望天：当无穷远处理
  }

  vec3 col = texture(uAtlas, atlasUV(dir)).rgb;

  // 团辉光：项目团方向上叠同色高斯柔光。
  // 力度必须压到"若隐若现"：径向光斑在暗背景上就是一个圆盘/行星（用户实拍翻车）
  for (int k = 0; k < 12; k++) {
    if (k >= uCCount) break;
    vec3 cd = normalize(uCPos[k] - uCenter + vec3(1e-3));
    float g = exp((dot(cd, dir) - 1.0) * 40.0) * uCWgt[k];
    col += hue2rgb(uCHue[k]) * g * 0.12;
  }

  outColor = vec4(col, 1.0);
}
`;

// 上采样 + ACES + 冷色深化 + 轻对比 + 暗角 + 抖动。
// 八轮教训：屏幕空间的 ridged 刻丝全是"描线"——用户点名去掉；
// 照片感的细节全部来自烘焙层，合成只做分级。
export const COMPOSITE_FS = `#version 300 es
precision highp float;
out vec4 outColor;
uniform highp sampler2D uTex;
uniform vec2 uRes;
uniform float uExposure;
uniform float uZoom;      // sceneRadius / camDist：缩放联动
uniform vec3 uRd;         // 视线方向：暗腔锚定世界朝向
uniform float uTime;

vec3 aces(vec3 x) {
  return clamp((x * (2.51 * x + 0.03)) / (x * (2.43 * x + 0.59) + 0.14), 0.0, 1.0);
}

float hash12(vec2 p) {
  vec3 p3 = fract(vec3(p.xyx) * 0.1031);
  p3 += dot(p3, p3.yzx + 33.33);
  return fract((p3.x + p3.y) * p3.z);
}

float vnoise2(vec2 p) {
  vec2 i = floor(p);
  vec2 f = fract(p);
  f = f * f * (3.0 - 2.0 * f);
  float a = hash12(i);
  float b = hash12(i + vec2(1.0, 0.0));
  float c = hash12(i + vec2(0.0, 1.0));
  float d = hash12(i + vec2(1.0, 1.0));
  return mix(mix(a, b, f.x), mix(c, d, f.x), f.y);
}

void main() {
  vec2 uv = gl_FragCoord.xy / uRes;
  vec3 c = texture(uTex, uv).rgb;

  // 暗部深化：只做极轻的低频暗示（暗尘带本体在烘焙层里）
  float voidN = vnoise2(uv * vec2(uRes.x / uRes.y, 1.0) * (4.2 / max(0.25, uZoom)) + uRd.xy * 2.0 + 31.0);
  c *= 0.66 + 0.34 * smoothstep(0.32, 0.78, voidN);

  c = max(c, 0.0);
  c = aces(c * uExposure);
  // 冷色深化：只对本来就偏蓝的像素加一点蓝青（力度收敛——靛蓝过深会
  // 偏离参考图的鲜亮青蓝）
  float isBlue = clamp((c.b - c.r) * 2.2, 0.0, 1.0);
  c = mix(c, c * vec3(0.90, 0.98, 1.12), isBlue * 0.35);
  // 轻度对比 S 曲线 + 饱和补偿（曲线过狠会重新出现"硬"感）
  float l2 = dot(c, vec3(0.2126, 0.7152, 0.0722));
  vec3 sc = c * c * (3.0 - 2.0 * c);
  c = mix(c, sc, 0.42);
  c = clamp(mix(vec3(l2), c, 1.55), 0.0, 1.0);
  // 暗角已删（九轮）：星云铺满画面后，圆形暗角 = "中间亮四周黑"，
  // 整个屏幕看起来就是一颗球（用户："还是个球"）。参考照片没有暗角，星到边。
  c += (hash12(gl_FragCoord.xy) - 0.5) / 255.0;        // 抖动
  outColor = vec4(max(c, 0.0), 1.0);
}
`;

export const STAR_VS = `#version 300 es
precision highp float;
layout(location=0) in vec3 aPos;
layout(location=1) in float aSize;
layout(location=2) in vec3 aColor;
layout(location=3) in float aBright;   // 亮度/暗淡系数（搜索过滤用）
layout(location=4) in float aSpike;    // >0.5 画十字星芒
uniform vec3 uRight;
uniform vec3 uUp;
uniform vec3 uFwd;
uniform vec3 uCam;
uniform vec2 uRes;
uniform float uTanFov;
uniform float uTime;
uniform float uSizeMul;  // 光点大小滑条（0.4~2.2）
out vec3 vColor;
out float vSpike;
out float vBright;
float hash13(vec3 p) {
  p = fract(p * 0.1031);
  p += dot(p, p.zyx + 31.32);
  return fract((p.x + p.y) * p.z);
}
void main() {
  vec3 rel = aPos - uCam;
  float z = dot(rel, uFwd);
  if (z <= 1.0) { gl_Position = vec4(2.0, 2.0, 2.0, 1.0); gl_PointSize = 1.0; return; }
  float aspect = uRes.x / uRes.y;
  float x = dot(rel, uRight) / (z * uTanFov * aspect);
  float y = dot(rel, uUp) / (z * uTanFov);
  gl_Position = vec4(x, y, 0.0, 1.0);
  float f = uRes.y / (2.0 * uTanFov);
  // 上限 220：光点整体加大后，贴脸的星也要能撑起大轮廓（用户："不要保守""还是不够"）
  gl_PointSize = clamp(aSize * uSizeMul * f / z, 2.5, 220.0);
  vColor = aColor;
  vSpike = aSpike;
  // 闪烁：每颗星按位置哈希取相位，幅度小，避免"圣诞灯"感
  float tw = 0.88 + 0.12 * sin(uTime * 0.0011 + hash13(aPos) * 60.0);
  vBright = aBright * tw;
}
`;

export const STAR_FS = `#version 300 es
precision highp float;
in vec3 vColor;
in float vSpike;
in float vBright;
out vec4 outColor;
uniform float uGlow;     // 光点发光程度滑条（0~2，只缩放辉光/星芒，不动亮核）
void main() {
  vec2 q = gl_PointCoord - 0.5;
  float r = length(q) * 2.0;
  if (r > 1.0) discard;
  // 三段式渐进辉光：锐利亮核为主 + 收敛的辉光（参考图里恒星只是锐利的小光点，
  // 大面积泛光的是云；光斑糊成球就是"刺眼"的来源）
  float core = exp(-r * r * 42.0);
  float glow = exp(-r * r * 9.0) * 0.34;
  float halo = exp(-r * r * 2.2) * 0.10;
  float spikes = 0.0;
  if (vSpike > 0.5) {
    vec2 a = abs(q) * 2.0;
    spikes = (exp(-a.y * 34.0) * exp(-a.x * 2.6) + exp(-a.x * 34.0) * exp(-a.y * 2.6)) * 0.5;
  }
  vec3 c = vColor * (core + glow * uGlow) + vColor * (halo * uGlow);
  c += vec3(1.0) * spikes * (0.32 * uGlow);
  float aa = core + (glow * 0.9 + halo + spikes * 0.6) * uGlow;
  outColor = vec4(c * vBright, aa * vBright);
}
`;
