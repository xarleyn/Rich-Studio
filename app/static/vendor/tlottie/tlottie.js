// Проигрывание .tgs-анимаций (телеграмные кастомные эмодзи) в предпросмотре.
// Движок — tlottie (github.com/dkaraush/tlottie, MIT), wasm собран автором в
// examples/web на коммите 92df98dc209bc39b1e567ec74a8c86a0af5239de (UPSTREAM).
// Это единственный wasm в студии, поэтому CSP script-src несёт 'wasm-unsafe-eval';
// файлы грузятся лениво — только когда в предпросмотре появился .tgs-эмодзи.
// Экспорт — window.Tlottie: play(canvas, bytes) -> stop(), stop(canvas).
/* global Tlottie */
(() => {
  'use strict';

  const SIMD = '/static/vendor/tlottie/tlottie.wasm';
  const NOSIMD = '/static/vendor/tlottie/tlottie.nosimd.wasm';
  let engine = null;          // { exports, memory } после загрузки
  let enginePromise = null;
  const playing = new Map();  // canvas -> { inst, raf, w, h, fps, frames }

  async function engine_() {
    if (engine) return engine;
    enginePromise ??= (async () => {
      let instance = null;
      try {
        ({ instance } = await WebAssembly.instantiateStreaming(fetch(SIMD), {}));
      } catch {
        // старый браузер без wasm-SIMD: сборка без SIMD
        ({ instance } = await WebAssembly.instantiateStreaming(fetch(NOSIMD), {}));
      }
      engine = { exports: instance.exports };
      return engine;
    })();
    return enginePromise;
  }

  const heap = () => new Uint8Array(engine.exports.memory.buffer);

  /** Оторванные от документа canvas останавливаются сами. */
  function sweep() {
    for (const [canvas] of [...playing]) if (!canvas.isConnected) stop(canvas);
  }

  async function play(canvas, bytes) {
    const e = await engine_();
    stop(canvas);
    const ptr = e.exports.tlottie_alloc(bytes.length);
    if (!ptr) throw new Error('tlottie: allocation failed');
    heap().set(bytes, ptr);
    const inst = e.exports.tlottie_new_with_options(ptr, bytes.length, 0, 0, 0);
    e.exports.tlottie_free(ptr, bytes.length);
    if (!inst) throw new Error('tlottie: файл не разобран');
    const w = e.exports.tlottie_width(inst);
    const h = e.exports.tlottie_height(inst);
    const fps = e.exports.tlottie_frame_rate(inst) || 30;
    const frames = e.exports.tlottie_frame_count(inst);
    canvas.width = w;
    canvas.height = h;
    const ctx = canvas.getContext('2d');
    const state = { inst, raf: 0, pos: 0, prev: 0, w, h, fps, frames };
    playing.set(canvas, state);
    const tick = (now) => {
      if (!playing.get(canvas) || !canvas.isConnected) { stop(canvas); return; }
      const dt = state.prev ? Math.min(now - state.prev, 100) : 0;
      state.prev = now;
      state.pos = (state.pos + (dt / 1000) * state.fps) % state.frames;
      const px = e.exports.tlottie_render_with_options(state.inst, state.pos % state.frames, state.w, state.h, 1, 0);
      if (px) {
        // память wasm могла вырасти: вид берём заново после каждого вызова
        const rgba = new Uint8ClampedArray(e.exports.memory.buffer, px, state.w * state.h * 4);
        ctx.putImageData(new ImageData(rgba, state.w, state.h), 0, 0);
      }
      state.raf = requestAnimationFrame(tick);
    };
    state.raf = requestAnimationFrame(tick);
    sweep();
  }

  function stop(canvas) {
    const state = playing.get(canvas);
    if (!state) return;
    cancelAnimationFrame(state.raf);
    try { engine?.exports.tlottie_drop(state.inst); } catch { /* уже сброшен */ }
    playing.delete(canvas);
  }

  window.Tlottie = { play, stop, loaded: () => Boolean(engine) };
})();
