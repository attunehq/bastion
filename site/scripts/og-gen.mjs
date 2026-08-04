// Generates the Open Graph card (public/og.png, 1200x630) from an on-brand
// HTML template. Run with: node scripts/og-gen.mjs
import { chromium } from "playwright";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const outPath = resolve(here, "../public/og.png");

const html = `<!doctype html><html><head><meta charset="utf-8">
<style>
  @import url('https://fonts.googleapis.com/css2?family=Schibsted+Grotesk:wght@400;600;700&family=JetBrains+Mono:wght@500;600&display=swap');
  * { margin: 0; box-sizing: border-box; }
  html, body { width: 1200px; height: 630px; }
  body {
    font-family: 'Schibsted Grotesk', system-ui, sans-serif;
    background-color: #f5f6f8;
    background-image:
      linear-gradient(rgba(80,90,110,0.05) 1px, transparent 1px),
      linear-gradient(90deg, rgba(80,90,110,0.05) 1px, transparent 1px);
    background-size: 40px 40px;
    color: #23262d;
    padding: 72px 80px;
    display: flex;
    flex-direction: column;
    justify-content: space-between;
  }
  .top { display: flex; align-items: center; gap: 14px; }
  .mark { width: 40px; height: 40px; }
  .word { font-family: 'JetBrains Mono', monospace; font-weight: 600; font-size: 30px; letter-spacing: -0.01em; }
  .tag {
    margin-left: auto; font-family: 'JetBrains Mono', monospace; font-size: 17px;
    color: #5a6473; border: 1px solid #c4ccd6; border-radius: 999px; padding: 8px 16px;
  }
  h1 {
    font-size: 74px; font-weight: 700; line-height: 1.04; letter-spacing: -0.035em;
    max-width: 17ch; color: #1f2229;
  }
  h1 .em { color: #2f5fa8; }
  .pills { display: flex; gap: 14px; align-items: center; }
  .pill {
    font-family: 'JetBrains Mono', monospace; font-weight: 600; font-size: 19px;
    padding: 9px 16px; border-radius: 6px; border: 1px solid; display: inline-flex; gap: 9px; align-items: center;
  }
  .dot { width: 9px; height: 9px; border-radius: 999px; background: currentColor; }
  .pass { color: #2f7a4d; background: #e6f4ec; border-color: #bfe2cd; }
  .block { color: #b23a2e; background: #fbe9e6; border-color: #f1c3bc; }
  .advisor { color: #946a1d; background: #f8efdb; border-color: #ecd9ad; }
  .foot { margin-left: auto; font-family: 'JetBrains Mono', monospace; font-size: 18px; color: #5a6473; }
</style></head>
<body>
  <div class="top">
    <svg class="mark" viewBox="0 0 32 32">
      <rect width="32" height="32" rx="6" fill="#23262d"/>
      <path fill="#f5f6f8" d="M6 9h4v3h3V9h6v3h3V9h4v14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2V9Z"/>
      <path fill="#23262d" d="M13 25v-5a3 3 0 0 1 6 0v5h-6Z"/>
      <path fill="none" stroke="#3ba06a" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" d="M13.2 16.1l1.9 1.9 3.7-3.9"/>
    </svg>
    <span class="word">bastion</span>
    <span class="tag">Open-source agentic code review</span>
  </div>

  <h1>A merge gate built from <span class="em">small reviewers</span> you write and own.</h1>

  <div class="pills">
    <span class="pill pass"><span class="dot"></span>pass</span>
    <span class="pill block"><span class="dot"></span>block</span>
    <span class="pill advisor"><span class="dot"></span>advisor</span>
    <span class="foot">bastion.attune.inc</span>
  </div>
</body></html>`;

const browser = await chromium.launch();
const page = await browser.newPage({ viewport: { width: 1200, height: 630 } });
await page.setContent(html, { waitUntil: "networkidle" });
await page.evaluate(() => document.fonts.ready);
await page.waitForTimeout(400);
await page.screenshot({ path: outPath });
await browser.close();
console.log("wrote", outPath);
