// Liquid Glass for the demo, in WebGL. The thread and the chat list are rasterized into
// textures (an inline-styled clone drawn through SVG <foreignObject>), re-taken only when
// their content changes; scrolling just moves the sampling window. One canvas over the
// demo draws every `.lg` shape: a convex squircle bezel bends the backdrop by Snell's law
// (after Chris Feijoo's "Liquid Glass in the browser", kube.io), over a frosted copy, with
// a little dispersion at the rim, a specular edge and a soft shadow. The shapes' own
// content sits above the canvas. Without WebGL2, or with reduced transparency, the CSS
// frosted glass stays.

const VERT_QUAD = `#version 300 es
in vec2 aPos;
uniform vec4 uBox;   // x, y, w, h in CSS px
uniform vec2 uView;  // canvas size in CSS px
out vec2 vPx;
void main() {
  vPx = uBox.xy + aPos * uBox.zw;
  vec2 clip = vPx / uView * 2.0 - 1.0;
  gl_Position = vec4(clip.x, -clip.y, 0.0, 1.0);
}`;

const VERT_FULL = `#version 300 es
in vec2 aPos;
out vec2 vUv;
void main() { vUv = aPos; gl_Position = vec4(aPos * 2.0 - 1.0, 0.0, 1.0); }`;

// Separable gaussian (σ in texels along uDir), for the frosted copy of a backdrop.
const FRAG_BLUR = `#version 300 es
precision highp float;
in vec2 vUv;
uniform sampler2D uTex;
uniform vec2 uDir;
uniform float uSigma;
out vec4 o;
void main() {
  vec4 sum = vec4(0.0); float total = 0.0;
  for (int i = -12; i <= 12; i++) {
    float x = float(i) * uSigma / 4.0;
    float w = exp(-0.5 * pow(x / uSigma, 2.0));
    sum += texture(uTex, vUv + uDir * x) * w; total += w;
  }
  o = sum / total;
}`;

const FRAG_GLASS = `#version 300 es
precision highp float;
in vec2 vPx;
uniform vec4 uRect;      // the glass shape, CSS px
uniform float uRadius, uBezel, uThick, uMix, uZoom;
uniform sampler2D uSharp, uFrost;
uniform vec4 uSrc;       // backdrop texture: origin in canvas px (scroll applied), size
uniform vec3 uBg;        // what the backdrop shows where it has no content
uniform vec2 uFade;      // scroll-edge fade: top height, bottom start (0 = none)
uniform float uViewH;
out vec4 o;

float sdBox(vec2 p, vec2 b, float r) {
  vec2 q = abs(p) - b + r;
  return length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - r;
}
float sd(vec2 p) { return sdBox(p - (uRect.xy + uRect.zw * 0.5), uRect.zw * 0.5, uRadius); }

// Lateral shift (px) of a vertical ray entering the bezel at x (0 rim … 1 flat): it bends
// towards the normal of a convex squircle surface and travels through the glass.
float shift(float x) {
  x = clamp(x, 0.002, 1.0);
  float s = 1.0 - pow(1.0 - x, 4.0);
  float h = pow(s, 0.25);
  float slope = pow(s, -0.75) * pow(1.0 - x, 3.0);
  float inc = atan(slope * uThick / uBezel);
  float refr = asin(sin(inc) / 1.5);
  return uThick * h * tan(inc - refr);
}

vec3 backdrop(sampler2D tex, vec2 px) {
  vec2 uv = (px - uSrc.xy) / uSrc.zw;
  vec3 c = uBg;
  if (all(greaterThanEqual(uv, vec2(0.0))) && all(lessThanEqual(uv, vec2(1.0)))) {
    vec4 t = texture(tex, uv);
    c = t.rgb + uBg * (1.0 - t.a);
  }
  // The same soft edges the page draws over the backdrop.
  float f = 0.0;
  if (uFade.x > 0.0) f = max(f, 0.95 * pow(clamp(1.0 - px.y / uFade.x, 0.0, 1.0), 0.7));
  if (uFade.y > 0.0) f = max(f, 0.92 * clamp((px.y - uFade.y) / (uViewH - uFade.y), 0.0, 1.0));
  return mix(c, uBg, f);
}

void main() {
  float d = sd(vPx);
  float e = 0.5;
  vec2 n = normalize(vec2(sd(vPx + vec2(e, 0)) - sd(vPx - vec2(e, 0)), sd(vPx + vec2(0, e)) - sd(vPx - vec2(0, e))) + 1e-6);
  float inset = -d;
  float bez = clamp(inset / uBezel, 0.0, 1.0);
  vec2 center = uRect.xy + uRect.zw * 0.5;
  // A slight magnification of the flat interior, plus the bezel's bend.
  vec2 off = (center - vPx) * uZoom + (inset < uBezel ? -n * shift(bez) : vec2(0.0));

  // Frosted body, sharper towards the rim where the bend carries the detail; R/G/B bend
  // a little differently (dispersion).
  vec3 frost = vec3(backdrop(uFrost, vPx + off * 0.94).r, backdrop(uFrost, vPx + off).g, backdrop(uFrost, vPx + off * 1.06).b);
  vec3 sharp = backdrop(uSharp, vPx + off);
  vec3 c = mix(sharp, frost, uMix * smoothstep(0.0, 0.7, bez));

  float l = dot(c, vec3(0.2126, 0.7152, 0.0722));
  c = mix(vec3(l), c, 1.45);           // saturation
  c = c * 0.9 + 0.075;                  // a light tint, like regular glass in dark mode

  vec2 light = normalize(vec2(-0.45, -1.0));
  float facing = dot(n, light);
  float rim = 1.0 - smoothstep(0.0, 1.6, inset);
  c += rim * (0.18 + 0.5 * pow(abs(facing), 3.0));
  c += pow(1.0 - bez, 3.0) * max(facing, 0.0) * 0.12;

  float cover = clamp(0.5 - d, 0.0, 1.0);
  float shadow = 0.1 * exp(-max(sd(vPx - vec2(0.0, 1.5)), 0.0) / 3.5) * (1.0 - cover);
  o = vec4(clamp(c, 0.0, 1.0) * cover, cover + shadow);
}`;

export function supported() {
  if (matchMedia('(prefers-reduced-transparency: reduce)').matches) return false;
  return !!document.createElement('canvas').getContext('webgl2');
}

function compile(gl, vs, fs) {
  const p = gl.createProgram();
  for (const [type, src] of [[gl.VERTEX_SHADER, vs], [gl.FRAGMENT_SHADER, fs]]) {
    const s = gl.createShader(type);
    gl.shaderSource(s, src); gl.compileShader(s);
    if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(s));
    gl.attachShader(p, s);
  }
  gl.bindAttribLocation(p, 0, 'aPos');
  gl.linkProgram(p);
  if (!gl.getProgramParameter(p, gl.LINK_STATUS)) throw new Error(gl.getProgramInfoLog(p));
  const u = {};
  for (let i = 0; i < gl.getProgramParameter(p, gl.ACTIVE_UNIFORMS); i++) {
    const name = gl.getActiveUniform(p, i).name;
    u[name] = gl.getUniformLocation(p, name);
  }
  return { p, u };
}

// Copies the computed style of every element onto its clone (no animations: an SVG image
// shows their first frame).
function inlineStyles(src, dst) {
  const cs = getComputedStyle(src);
  for (let i = 0; i < cs.length; i++) dst.style.setProperty(cs[i], cs.getPropertyValue(cs[i]));
  dst.style.animation = 'none'; dst.style.transition = 'none';
  for (let i = 0; i < src.children.length; i++) inlineStyles(src.children[i], dst.children[i]);
}

// Draws the full scrollable content of `el` into a canvas, as `el` would show it.
async function rasterize(el, maxSize) {
  const w = el.clientWidth, h = el.scrollHeight;
  if (!w || !h) return null;
  const clone = el.cloneNode(true);
  inlineStyles(el, clone);
  Object.assign(clone.style, { position: 'static', inset: 'auto', margin: '0', width: `${w}px`, height: `${h}px`, overflow: 'visible', boxSizing: 'border-box' });
  const xml = new XMLSerializer().serializeToString(clone);
  const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="${w}" height="${h}"><foreignObject width="100%" height="100%">${xml}</foreignObject></svg>`;
  const img = new Image();
  img.src = `data:image/svg+xml;charset=utf-8,${encodeURIComponent(svg)}`;
  await img.decode();
  const scale = Math.min(devicePixelRatio || 1, 2, maxSize / w, maxSize / h);
  const canvas = document.createElement('canvas');
  canvas.width = Math.max(1, Math.round(w * scale)); canvas.height = Math.max(1, Math.round(h * scale));
  canvas.getContext('2d').drawImage(img, 0, 0, canvas.width, canvas.height);
  return { canvas, w, h };
}

/**
 * Draws Liquid Glass for every `.lg` element in `root` over `sources`: the scrolling
 * elements behind them, each `{ el, bg: [r,g,b], fade: (viewH) => [top, bottomStart] }`.
 * Each shape samples the source whose box contains it. Returns false if unsupported.
 */
export function liquidGlass(root, sources, { bezel = 16, thickness = 34, frost = 3, mix = 0.7, zoom = 0.03 } = {}) {
  if (!supported()) return false;
  const canvas = document.createElement('canvas');
  canvas.className = 'd-glass';
  canvas.setAttribute('aria-hidden', 'true');
  const gl = canvas.getContext('webgl2', { premultipliedAlpha: true, antialias: false });
  if (!gl) return false;
  let glassProg, blurProg;
  try {
    glassProg = compile(gl, VERT_QUAD, FRAG_GLASS);
    blurProg = compile(gl, VERT_FULL, FRAG_BLUR);
  } catch (err) {
    console.warn('liquid glass disabled:', err);
    return false;
  }
  root.append(canvas);
  root.classList.add('gl');

  const quad = gl.createBuffer();
  gl.bindBuffer(gl.ARRAY_BUFFER, quad);
  gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([0, 0, 1, 0, 0, 1, 1, 1]), gl.STATIC_DRAW);
  gl.enableVertexAttribArray(0);
  gl.vertexAttribPointer(0, 2, gl.FLOAT, false, 0, 0);
  const maxSize = Math.min(gl.getParameter(gl.MAX_TEXTURE_SIZE), 8192);

  const texture = () => {
    const t = gl.createTexture();
    gl.bindTexture(gl.TEXTURE_2D, t);
    for (const [k, v] of [[gl.TEXTURE_MIN_FILTER, gl.LINEAR], [gl.TEXTURE_MAG_FILTER, gl.LINEAR], [gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE], [gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE]]) gl.texParameteri(gl.TEXTURE_2D, k, v);
    return t;
  };
  const target = (w, h) => {
    const tex = texture();
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, w, h, 0, gl.RGBA, gl.UNSIGNED_BYTE, null);
    const fb = gl.createFramebuffer();
    gl.bindFramebuffer(gl.FRAMEBUFFER, fb);
    gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0, gl.TEXTURE_2D, tex, 0);
    return { tex, fb };
  };

  const state = sources.map(s => ({ ...s, sharp: null, frost: null, w: 0, h: 0, version: 0 }));
  let version = 0;

  async function snapshot(src) {
    const shot = await rasterize(src.el, maxSize).catch(err => { console.warn('liquid glass snapshot failed:', err); return null; });
    if (!shot) return;
    const { canvas: c } = shot;
    for (const t of [src.sharp, src.frost?.tex, src.tmp?.tex]) if (t) gl.deleteTexture(t);
    for (const f of [src.frost?.fb, src.tmp?.fb]) if (f) gl.deleteFramebuffer(f);
    src.sharp = texture();
    gl.pixelStorei(gl.UNPACK_PREMULTIPLY_ALPHA_WEBGL, true);
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, c);
    // Frosted copy: two gaussian passes.
    src.tmp = target(c.width, c.height);
    src.frost = target(c.width, c.height);
    gl.useProgram(blurProg.p);
    gl.disable(gl.BLEND);
    gl.viewport(0, 0, c.width, c.height);
    const sigma = frost * c.width / shot.w;
    gl.uniform1f(blurProg.u.uSigma, sigma);
    gl.uniform1i(blurProg.u.uTex, 0);
    gl.activeTexture(gl.TEXTURE0);
    for (const [from, to, dir] of [[src.sharp, src.tmp, [1 / c.width, 0]], [src.tmp.tex, src.frost, [0, 1 / c.height]]]) {
      gl.bindFramebuffer(gl.FRAMEBUFFER, to.fb);
      gl.bindTexture(gl.TEXTURE_2D, from);
      gl.uniform2f(blurProg.u.uDir, dir[0], dir[1]);
      gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
    }
    gl.bindFramebuffer(gl.FRAMEBUFFER, null);
    src.w = shot.w; src.h = shot.h;
    src.version = ++version;
  }

  // Re-take a source when its content changes: once right away and once after entry
  // animations settle.
  for (const src of state) {
    let timers = [];
    const schedule = () => {
      timers.forEach(clearTimeout);
      timers = [setTimeout(() => snapshot(src), 30), setTimeout(() => snapshot(src), 420)];
    };
    new MutationObserver(schedule).observe(src.el, { childList: true, subtree: true, characterData: true, attributes: true, attributeFilter: ['class'] });
    new ResizeObserver(schedule).observe(src.el);
    schedule();
  }
  document.fonts?.ready.then(() => state.forEach(snapshot));

  const shapes = [...root.querySelectorAll('.lg')];
  let last = '', visible = true;
  new IntersectionObserver(([entry]) => { visible = entry.isIntersecting; }).observe(root);

  function frame() {
    requestAnimationFrame(frame);
    if (!visible || document.hidden) return;
    const box = root.getBoundingClientRect();
    const dpr = Math.min(devicePixelRatio || 1, 2);
    const cw = Math.round(box.width * dpr), ch = Math.round(box.height * dpr);
    const items = [];
    for (const el of shapes) {
      const r = el.getBoundingClientRect();
      if (!r.width || !r.height || getComputedStyle(el).visibility === 'hidden') continue;
      const cx = r.left + r.width / 2, cy = r.top + r.height / 2;
      const src = state.find(s => {
        const b = s.el.getBoundingClientRect();
        return cx >= b.left && cx <= b.right && cy >= b.top && cy <= b.bottom;
      });
      if (!src?.sharp) continue;
      const b = src.el.getBoundingClientRect();
      const radius = Math.min(parseFloat(getComputedStyle(el).borderTopLeftRadius) || 0, r.width / 2, r.height / 2);
      items.push({ x: r.left - box.left, y: r.top - box.top, w: r.width, h: r.height, radius, src,
        ox: b.left - box.left - src.el.scrollLeft, oy: b.top - box.top - src.el.scrollTop });
    }
    const key = `${cw}x${ch}|${items.map(i => [i.x, i.y, i.w, i.h, i.ox, i.oy, i.src.version].map(v => v.toFixed(1)).join(',')).join(';')}`;
    if (key === last) return;
    last = key;
    if (canvas.width !== cw || canvas.height !== ch) { canvas.width = cw; canvas.height = ch; }
    gl.bindFramebuffer(gl.FRAMEBUFFER, null);
    gl.viewport(0, 0, cw, ch);
    gl.clearColor(0, 0, 0, 0);
    gl.clear(gl.COLOR_BUFFER_BIT);
    gl.enable(gl.BLEND);
    gl.blendFunc(gl.ONE, gl.ONE_MINUS_SRC_ALPHA);
    gl.useProgram(glassProg.p);
    const u = glassProg.u;
    gl.uniform2f(u.uView, box.width, box.height);
    gl.uniform1f(u.uViewH, box.height);
    gl.uniform1i(u.uSharp, 0);
    gl.uniform1i(u.uFrost, 1);
    for (const i of items) {
      const margin = 16;
      const bz = Math.min(bezel, i.w / 2, i.h / 2);
      gl.uniform4f(u.uBox, i.x - margin, i.y - margin, i.w + 2 * margin, i.h + 2 * margin);
      gl.uniform4f(u.uRect, i.x, i.y, i.w, i.h);
      gl.uniform1f(u.uRadius, i.radius);
      gl.uniform1f(u.uBezel, bz);
      gl.uniform1f(u.uThick, thickness * bz / bezel);
      gl.uniform1f(u.uMix, mix);
      gl.uniform1f(u.uZoom, zoom);
      gl.uniform4f(u.uSrc, i.ox, i.oy, i.src.w, i.src.h);
      gl.uniform3fv(u.uBg, i.src.bg);
      const [top, bottom] = i.src.fade(box.height);
      gl.uniform2f(u.uFade, top, bottom);
      gl.activeTexture(gl.TEXTURE0); gl.bindTexture(gl.TEXTURE_2D, i.src.sharp);
      gl.activeTexture(gl.TEXTURE1); gl.bindTexture(gl.TEXTURE_2D, i.src.frost.tex);
      gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
    }
  }
  requestAnimationFrame(frame);
  return true;
}
