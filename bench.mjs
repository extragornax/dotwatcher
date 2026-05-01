import { chromium } from 'playwright';

const URL = process.argv[2] || 'http://127.0.0.1:8081/event/rentas-desa-big-loop-666-26';

(async () => {
  const browser = await chromium.launch({
    executablePath: '/snap/bin/chromium',
    headless: true,
    args: ['--no-sandbox', '--disable-gpu'],
  });

  const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const page = await context.newPage();

  console.log(`Navigating to ${URL} …`);
  const navStart = Date.now();

  await page.goto(URL, { waitUntil: 'domcontentloaded' });
  const domReady = Date.now() - navStart;
  console.log(`DOM content loaded: ${domReady} ms`);

  // Wait for data to be usable: first participant row rendered
  let dataUsable = null;
  try {
    await page.waitForFunction(() => {
      // dotwatcher: .p inside #plist; madcap.cc: various participant selectors
      const dw = document.querySelectorAll('#plist .p');
      if (dw.length > 0) return true;
      const mc = document.querySelectorAll('[class*="participant"], [class*="Participant"], .ant-table-row, tr[data-id]');
      if (mc.length > 0) return true;
      const markers = document.querySelectorAll('.mapboxgl-marker, .leaflet-marker-icon');
      if (markers.length > 5) return true;
      return false;
    }, { timeout: 60000, polling: 100 });
    dataUsable = Date.now() - navStart;
    console.log(`Data usable (first row/marker visible): ${dataUsable} ms`);
  } catch {
    console.log('Data usable: TIMEOUT (60s)');
  }

  // Wait for participants to be fully rendered
  let participantsRendered = null;
  try {
    await page.waitForFunction(() => {
      // dotwatcher: .p rows in #plist; madcap.cc: participant/marker elements
      const dw = document.querySelectorAll('#plist .p');
      if (dw.length > 5) return true;
      const mc = document.querySelectorAll('[class*="participant"], [class*="Participant"], .ant-table-row');
      if (mc.length > 5) return true;
      const markers = document.querySelectorAll('.mapboxgl-marker, .leaflet-marker-icon');
      if (markers.length > 10) return true;
      return false;
    }, { timeout: 60000, polling: 200 });
    participantsRendered = Date.now() - navStart;
    console.log(`Participants rendered: ${participantsRendered} ms`);
  } catch {
    console.log('Participants rendered: TIMEOUT (60s)');
  }

  // Collect network timing for API requests
  const perfEntries = await page.evaluate(() => {
    return performance.getEntriesByType('resource')
      .filter(e => e.name.includes('/api/') || e.name.includes('api.madcap.cc'))
      .map(e => ({
        name: e.name.length > 120 ? e.name.slice(0, 120) + '…' : e.name,
        duration: Math.round(e.duration),
        transferSize: e.transferSize,
        startTime: Math.round(e.startTime),
      }));
  });

  console.log('\n--- API requests ---');
  for (const e of perfEntries) {
    const sizeMB = e.transferSize ? (e.transferSize / 1024 / 1024).toFixed(2) + ' MB' : '?';
    console.log(`  ${e.startTime}ms +${e.duration}ms  ${sizeMB}  ${e.name}`);
  }

  console.log('\n--- Summary ---');
  console.log(`DOM content loaded:    ${domReady} ms`);
  console.log(`Data usable:           ${dataUsable ? dataUsable + ' ms' : 'TIMEOUT'}`);
  console.log(`Participants rendered:  ${participantsRendered ? participantsRendered + ' ms' : 'TIMEOUT'}`);

  await browser.close();
})();
