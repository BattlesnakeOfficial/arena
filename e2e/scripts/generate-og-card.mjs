import { chromium } from '@playwright/test';
import { readFile, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';

// Mirrors GOOGLE_FONTS_HREF in server/src/components/page.rs.
const GOOGLE_FONTS_HREF = 'https://fonts.googleapis.com/css2?family=Bricolage+Grotesque:opsz,wght@12..96,300;12..96,500;12..96,600;12..96,700;12..96,800&family=Instrument+Sans:ital,wght@0,400;0,500;0,600;1,400&family=IBM+Plex+Mono:wght@400;500;600&display=swap';
// Mirrors DEFAULT_DESCRIPTION in server/src/components/page.rs.
const TAGLINE = 'A competitive arena where your code battles other Battlesnakes.';

const rustSourceUrl = new URL('../../server/src/components/page.rs', import.meta.url);
const outputUrl = new URL('../../server/static/og-card.png', import.meta.url);
const rustSource = await readFile(rustSourceUrl, 'utf8');

for (const [constant, value] of [
  ['GOOGLE_FONTS_HREF', GOOGLE_FONTS_HREF],
  ['DEFAULT_DESCRIPTION', TAGLINE],
]) {
  if (!rustSource.includes(`const ${constant}: &str = "${value}";`)) {
    throw new Error(
      `${constant} no longer matches server/src/components/page.rs; update the card generator when the Rust constants change.`,
    );
  }
}

const browser = await chromium.launch();
try {
  const page = await browser.newPage({ viewport: { width: 1200, height: 630 } });
  await page.setContent(
    `<!doctype html>
    <html><head><link href="${GOOGLE_FONTS_HREF}" rel="stylesheet"><style>
      * { box-sizing: border-box; }
      html, body { width: 1200px; height: 630px; margin: 0; }
      body { background: #17141A; color: #FCFBF8; display: flex; align-items: center; padding: 92px 96px; }
      main { display: flex; flex-direction: column; gap: 64px; width: 100%; }
      .lockup { display: flex; align-items: center; gap: 34px; font-family: "Bricolage Grotesque", sans-serif; font-size: 88px; font-weight: 700; letter-spacing: -3px; }
      svg { width: 142px; height: 142px; flex: none; }
      p { margin: 0; max-width: 930px; font-family: "Instrument Sans", sans-serif; font-size: 38px; font-weight: 400; line-height: 1.25; color: rgba(252, 251, 248, .78); }
    </style></head><body><main>
      <div class="lockup">
        <svg viewBox="0 0 24 24" fill="none" aria-hidden="true">
          <rect x="2" y="2" width="9" height="9" rx="2.5" fill="#E8256D"/>
          <rect x="13" y="2" width="9" height="9" rx="2.5" fill="#FCFBF8" opacity=".2"/>
          <rect x="2" y="13" width="9" height="9" rx="2.5" fill="#FCFBF8" opacity=".2"/>
          <rect x="13" y="13" width="9" height="9" rx="2.5" fill="#E8256D" opacity=".45"/>
        </svg>
        <span>Battlesnake Arena</span>
      </div>
      <p>${TAGLINE}</p>
    </main></body></html>`,
    { waitUntil: 'networkidle' },
  );
  await page.evaluate(() => document.fonts.ready);
  const fontsLoaded = await page.evaluate(() => ({
    lockup: document.fonts.check('700 88px "Bricolage Grotesque"'),
    tagline: document.fonts.check('400 38px "Instrument Sans"'),
  }));
  if (!fontsLoaded.lockup || !fontsLoaded.tagline) {
    throw new Error(`Required Google Fonts failed to load: ${JSON.stringify(fontsLoaded)}`);
  }

  const png = await page.screenshot({ type: 'png' });
  const signature = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
  if (!png.subarray(0, 8).equals(signature)) throw new Error('Screenshot is not a PNG');
  const width = png.readUInt32BE(16);
  const height = png.readUInt32BE(20);
  if (width !== 1200 || height !== 630) {
    throw new Error(`Expected 1200x630 PNG, got ${width}x${height}`);
  }
  if (png.length > 153_600) {
    throw new Error(`PNG is ${png.length} bytes; maximum is 153600 bytes`);
  }
  await writeFile(fileURLToPath(outputUrl), png);
  console.log(`Wrote ${fileURLToPath(outputUrl)} (${png.length} bytes)`);
} finally {
  await browser.close();
}
