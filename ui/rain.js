// Decorative "digital rain" behind the UI. It draws only local glyphs, makes
// no network requests, pauses with the window, and stays still when the user
// prefers reduced motion.
(() => {
  const canvas = document.querySelector("#matrix-rain");
  const context = canvas?.getContext("2d");
  if (!context) {
    return;
  }

  const glyphs =
    "アイウエオカキクケコサシスセソタチツテトナニヌネノハヒフヘホマミムメモヤユヨラリルレロワン0123456789HLS:/.<>=+*";
  const fontSize = 16;
  const frameInterval = 1000 / 24;
  const reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)");
  let drops = [];
  let width = 0;
  let height = 0;
  let animation = 0;
  let lastFrame = 0;

  function randomGlyph() {
    return glyphs[Math.floor(Math.random() * glyphs.length)];
  }

  function resize() {
    const ratio = Math.min(window.devicePixelRatio || 1, 2);
    width = window.innerWidth;
    height = window.innerHeight;
    canvas.width = Math.floor(width * ratio);
    canvas.height = Math.floor(height * ratio);
    context.setTransform(ratio, 0, 0, ratio, 0, 0);
    const rows = Math.ceil(height / fontSize);
    // Start some columns mid-screen so the rain is visible at once.
    drops = Array.from({ length: Math.ceil(width / fontSize) }, () =>
      Math.floor(Math.random() * rows * 2) - rows,
    );
    context.fillStyle = "#000300";
    context.fillRect(0, 0, width, height);
  }

  function draw() {
    context.fillStyle = "rgba(0, 3, 0, 0.09)";
    context.fillRect(0, 0, width, height);
    context.font = `${fontSize}px ui-monospace, "DejaVu Sans Mono", monospace`;
    context.textBaseline = "top";

    for (let column = 0; column < drops.length; column += 1) {
      const x = column * fontSize;
      const y = drops[column] * fontSize;
      if (y >= 0) {
        // Re-ink the cell above in green so the bright head leaves a trail.
        context.fillStyle = "#00ff41";
        context.fillText(randomGlyph(), x, y - fontSize);
        context.fillStyle = "#d8ffe2";
        context.fillText(randomGlyph(), x, y);
      }
      if (y > height && Math.random() > 0.975) {
        drops[column] = 0;
      } else {
        drops[column] += 1;
      }
    }
  }

  function loop(time) {
    if (time - lastFrame >= frameInterval) {
      lastFrame = time;
      draw();
    }
    animation = window.requestAnimationFrame(loop);
  }

  function start() {
    window.cancelAnimationFrame(animation);
    resize();
    if (reducedMotion.matches) {
      // One still frame of rain instead of motion.
      for (let step = 0; step < 60; step += 1) {
        draw();
      }
      return;
    }
    animation = window.requestAnimationFrame(loop);
  }

  let resizeTimer = 0;
  window.addEventListener("resize", () => {
    window.clearTimeout(resizeTimer);
    resizeTimer = window.setTimeout(start, 150);
  });
  reducedMotion.addEventListener("change", start);
  start();
})();
