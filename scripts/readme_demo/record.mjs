// Records the README demos: the real panel UI (gui/ui) with the synthetic
// data in mock.js, light and dark side by side.
//
//   node scripts/readme_demo/record.mjs [--scene home|tour] [--fps N] [--chrome PATH] [--out FILE]
//     [--still [--at MS,MS...]]
//
// `home` (assets/coport-demo.gif) stays on Home while calls arrive; `tour`
// (assets/coport-pages.gif) clicks through Home, Activity and Devices.
//
// Needs Google Chrome (or Chromium), ffmpeg and gifsicle. Nothing is contacted besides
// a loopback file server; the browser runs with a temporary profile.
import { spawn, execFileSync } from "node:child_process";
import { createServer } from "node:http";
import { mkdtempSync, readFileSync, rmSync, writeFileSync, mkdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, extname, join, normalize, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, "../..");
const UI = join(ROOT, "gui/ui");

const args = process.argv.slice(2);
const option = (name, fallback) => {
  const i = args.indexOf(name);
  return i >= 0 ? args[i + 1] : fallback;
};
const CHROME = option("--chrome", process.platform === "darwin"
  ? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
  : "google-chrome");
// The tour's page changes last under half a second; more frames keep them fluid.
const SCENES = { home: { file: "coport-demo.gif", seconds: 10, fps: 12 }, tour: { file: "coport-pages.gif", seconds: 14, fps: Number(option("--fps", 20)) } };
const SCENE = option("--scene", "home");
if (!SCENES[SCENE]) throw new Error(`Unknown scene ${SCENE}; use ${Object.keys(SCENES).join(" or ")}`);
const OUT = resolve(option("--out", join(ROOT, "assets", SCENES[SCENE].file)));
const STILL = args.includes("--still");

const WIDTH = 360; // The panel's window width (gui/tauri.conf.json).
// From the header through Outbound Proxies.
const HEIGHT = Number(option("--height", 898));
const SCALE = 2;
const FPS = SCENES[SCENE].fps;
const SECONDS = SCENES[SCENE].seconds;
const THEMES = ["light", "dark"];

// ------------------------------------------------------------ file server

const TYPES = { ".html": "text/html", ".js": "text/javascript", ".css": "text/css", ".svg": "image/svg+xml", ".png": "image/png" };
// The panel's popover material is transparent in a browser; give it the window color.
const STYLE = `<style>.macos .panel { background: var(--window); border-radius: 0; }</style>`;
const server = createServer((req, res) => {
  const path = new URL(req.url, "http://localhost").pathname;
  if (path === "/mock.js") {
    res.writeHead(200, { "content-type": TYPES[".js"] });
    return res.end(readFileSync(join(HERE, "mock.js")));
  }
  const file = normalize(join(UI, path === "/" ? "index.html" : path));
  if (!file.startsWith(UI)) {
    res.writeHead(403);
    return res.end();
  }
  try {
    let body = readFileSync(file);
    if (file.endsWith("index.html")) {
      body = body.toString().replace('<script src="app.js"', `${STYLE}<script src="/mock.js"></script><script src="app.js"`);
    }
    res.writeHead(200, { "content-type": TYPES[extname(file)] ?? "application/octet-stream" });
    res.end(body);
  } catch {
    res.writeHead(404);
    res.end();
  }
});
await new Promise((ok) => server.listen(0, "127.0.0.1", ok));
const pageUrl = `http://127.0.0.1:${server.address().port}/?scene=${SCENE}`;

// ----------------------------------------------------------------- chrome

const work = mkdtempSync(join(tmpdir(), "coport-demo-"));
const chrome = spawn(CHROME, [
  "--headless=new", "--remote-debugging-port=0", `--user-data-dir=${join(work, "profile")}`,
  "--no-first-run", "--no-default-browser-check", "--disable-extensions", "--hide-scrollbars",
  "--force-color-profile=srgb", "about:blank",
], { stdio: ["ignore", "ignore", "pipe"] });
const endpoint = await new Promise((ok, fail) => {
  let log = "";
  chrome.stderr.on("data", (chunk) => {
    log += chunk;
    const match = log.match(/DevTools listening on (ws:\/\/\S+)/);
    if (match) ok(match[1]);
  });
  chrome.on("exit", (code) => fail(new Error(`Chrome exited (${code}):\n${log}`)));
});

const socket = new WebSocket(endpoint);
await new Promise((ok) => socket.addEventListener("open", ok, { once: true }));
let nextId = 0;
const pending = new Map();
const waiters = [];
socket.addEventListener("message", ({ data }) => {
  const message = JSON.parse(data);
  if (message.id != null) {
    const { ok, fail } = pending.get(message.id);
    pending.delete(message.id);
    message.error ? fail(new Error(JSON.stringify(message.error))) : ok(message.result);
  } else {
    for (const w of waiters.splice(0)) if (!w(message)) waiters.push(w);
  }
});
const send = (method, params = {}, sessionId) => new Promise((ok, fail) => {
  const id = ++nextId;
  pending.set(id, { ok, fail });
  socket.send(JSON.stringify({ id, method, params, sessionId }));
});
const event = (method, sessionId) => new Promise((ok) => {
  waiters.push((m) => (m.method === method && m.sessionId === sessionId ? (ok(m.params), true) : false));
});

async function openPage(theme) {
  const { targetId } = await send("Target.createTarget", { url: "about:blank" });
  const { sessionId } = await send("Target.attachToTarget", { targetId, flatten: true });
  const on = (method, params) => send(method, params, sessionId);
  await on("Page.enable");
  await on("Emulation.setDeviceMetricsOverride", { width: WIDTH, height: HEIGHT, deviceScaleFactor: SCALE, mobile: false });
  await on("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: theme }] });
  const loaded = event("Page.loadEventFired", sessionId);
  await on("Page.navigate", { url: pageUrl });
  await loaded;
  return on;
}

async function frame(on, elapsed, file) {
  const result = await on("Runtime.evaluate", { expression: `__demo.at(${elapsed})`, awaitPromise: true });
  if (result.exceptionDetails) throw new Error(JSON.stringify(result.exceptionDetails));
  const { data } = await on("Page.captureScreenshot", { format: "png" });
  writeFileSync(file, Buffer.from(data, "base64"));
}

try {
  const frames = STILL ? option("--at", String(SECONDS * 1000)).split(",").map(Number) : Array.from({ length: FPS * SECONDS }, (_, i) => Math.round((i * 1000) / FPS));
  for (const theme of THEMES) {
    const on = await openPage(theme);
    mkdirSync(join(work, theme));
    for (const [i, elapsed] of frames.entries()) {
      await frame(on, elapsed, join(work, theme, `${String(i).padStart(4, "0")}.png`));
    }
  }
  mkdirSync(dirname(OUT), { recursive: true });
  const inputs = THEMES.flatMap((theme) => ["-framerate", String(FPS), "-i", join(work, theme, "%04d.png")]);
  if (STILL) {
    // One image per requested time: frame.png, or frame-1.png, frame-2.png, …
    frames.forEach((_, i) => {
      const name = String(i).padStart(4, "0");
      const out = frames.length > 1 ? OUT.replace(/(\.\w+)?$/, `-${i + 1}$1`) : OUT;
      execFileSync("ffmpeg", ["-v", "error", "-y", ...THEMES.flatMap((theme) => ["-i", join(work, theme, `${name}.png`)]), "-filter_complex", "hstack", out]);
    });
  } else {
    // One palette for the whole clip keeps colors steady; only changed pixels are re-encoded.
    execFileSync("ffmpeg", [
      "-v", "error", "-y", ...inputs, "-filter_complex",
      "[0][1]hstack,split[a][b];[a]palettegen=max_colors=128:stats_mode=full[p];[b][p]paletteuse=dither=none:diff_mode=rectangle",
      "-loop", "0", OUT,
    ]);
    // Page changes redraw whole frames; slight lossiness keeps the GIF small
    // without visible noise on text.
    execFileSync("gifsicle", ["-O3", "--lossy=80", "--batch", OUT]);
  }
  console.log(OUT);
} catch (error) {
  console.error(error);
  process.exitCode = 1;
} finally {
  socket.close();
  // The profile can only be removed once Chrome has let go of it.
  if (chrome.exitCode === null && chrome.signalCode === null) {
    const exited = new Promise((ok) => chrome.once("exit", ok));
    chrome.kill();
    await Promise.race([exited, new Promise((ok) => setTimeout(ok, 5000))]);
  }
  server.close();
  rmSync(work, { recursive: true, force: true });
}
// A closing WebSocket can keep the event loop alive after Chrome is gone.
process.exit();
