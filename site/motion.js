/* rcmake shared motion controller. Reduced-motion aware. MIT OR Apache-2.0. */

export function prefersReducedMotion() {
  return typeof window !== "undefined" && typeof window.matchMedia === "function" && window.matchMedia("(prefers-reduced-motion: reduce)").matches;
}

export function animateCountUp(el, target, { duration = 800, format } = {}) {
  if (!el || typeof target !== "number" || !Number.isFinite(target)) return;
  const fmt = format || ((n) => Math.round(n).toLocaleString("en-US"));
  if (prefersReducedMotion() || typeof requestAnimationFrame !== "function") {
    el.textContent = fmt(target);
    return;
  }
  const start = performance.now();
  const step = (now) => {
    const t = Math.min(1, (now - start) / duration);
    el.textContent = fmt(target * (1 - Math.pow(1 - t, 3)));
    if (t < 1) requestAnimationFrame(step);
    else el.textContent = fmt(target);
  };
  requestAnimationFrame(step);
}

export function createMotionController(root = document) {
  const reduced = prefersReducedMotion();
  const callbacks = new WeakMap();
  let revealIO = null;
  let loopIO = null;

  const reveal = (el) => {
    if (el.classList.contains("is-in")) return;
    el.classList.add("is-in");
    const cb = callbacks.get(el);
    if (cb) { cb(el); callbacks.delete(el); }
  };

  if (typeof IntersectionObserver !== "undefined" && !reduced) {
    revealIO = new IntersectionObserver((entries) => {
      for (const e of entries) {
        if (e.isIntersecting && (e.intersectionRatio >= 0.35 || e.boundingClientRect.height > window.innerHeight * 0.72)) {
          reveal(e.target);
          revealIO.unobserve(e.target);
        }
      }
    }, { threshold: [0.05, 0.35] });
    loopIO = new IntersectionObserver((entries) => {
      for (const e of entries) e.target.classList.toggle("is-visible", e.isIntersecting);
    }, { threshold: 0.05 });
  }

  const observeReveal = (el, cb) => {
    if (!el) return;
    if (cb) callbacks.set(el, cb);
    if (reduced || !revealIO) reveal(el);
    else revealIO.observe(el);
  };
  const observeLoop = (el) => { if (el && !reduced && loopIO) loopIO.observe(el); };
  const scan = (scope = root) => {
    scope.querySelectorAll("[data-animate]").forEach((el) => { if (!el.classList.contains("is-in")) observeReveal(el); });
    scope.querySelectorAll("[data-loop]").forEach(observeLoop);
  };
  scan(root);
  return { reduced, observeReveal, observeLoop, scan };
}
