// Records a `fab-bench race` from inside both browsers (DevTools screenshots,
// no screen-recording permission needed): runs the race, grabs each side's
// page ~6 times a second, and timestamps the race's output lines.
//   node record.mjs <out dir> <fab-bench race args…>
import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";

const [out, ...raceArgs] = process.argv.slice(2);
for (const s of ["left", "right"]) fs.mkdirSync(path.join(out, s), { recursive: true });
const log = fs.createWriteStream(path.join(out, "race.log"));

// The race's two browser windows: Chrome with a DevTools port, placed at the
// top of the screen (the dashboard window sits below them).
function browsers() {
  const found = [];
  for (const l of execSync("ps -axo args", { maxBuffer: 1 << 24 }).toString().split("\n")) {
    if (!l.includes("--remote-debugging-port") || l.includes("--type=")) continue;
    const dir = /--user-data-dir=(\S+)/.exec(l)?.[1];
    const pos = /--window-position=(-?\d+),(-?\d+)/.exec(l);
    if (!dir || !pos || +pos[2] > 400) continue;
    let port;
    try { port = fs.readFileSync(path.join(dir, "DevToolsActivePort"), "utf8").split("\n")[0].trim(); } catch { continue; }
    found.push({ side: +pos[1] < 100 ? "left" : "right", port });
  }
  return found;
}

// One side: follow its fixture page and screenshot it in a loop.
async function capture(side, stop) {
  let ws = null, target = null, id = 0;
  const pending = new Map();
  const call = (method, params = {}) => new Promise((res) => {
    const n = ++id;
    pending.set(n, res);
    ws.send(JSON.stringify({ id: n, method, params }));
    setTimeout(() => pending.delete(n) && res(null), 3000);
  });
  while (!stop.done) {
    try {
      const b = browsers().find((x) => x.side === side);
      if (!b) { await new Promise((r) => setTimeout(r, 300)); continue; }
      const pages = (await (await fetch(`http://127.0.0.1:${b.port}/json/list`)).json()).filter((t) => t.type === "page");
      const t = pages.find((p) => p.url.startsWith("http")) || pages[0];
      if (!t) { await new Promise((r) => setTimeout(r, 300)); continue; }
      if (!ws || t.id !== target) {
        if (ws) ws.close();
        target = t.id;
        ws = new WebSocket(t.webSocketDebuggerUrl);
        ws.onmessage = (m) => { const d = JSON.parse(m.data); pending.get(d.id)?.(d.result); pending.delete(d.id); };
        await new Promise((r, j) => { ws.onopen = r; ws.onerror = j; });
      }
      const at = Date.now();
      const r = await call("Page.captureScreenshot", { format: "jpeg", quality: 70 });
      if (r?.data) fs.writeFileSync(path.join(out, side, `${at}.jpg`), Buffer.from(r.data, "base64"));
      await new Promise((r) => setTimeout(r, 120));
    } catch {
      ws = null;
      await new Promise((r) => setTimeout(r, 300));
    }
  }
  ws?.close();
}

const stop = { done: false };
const loops = [capture("left", stop), capture("right", stop)];
const race = spawn(raceArgs[0], raceArgs.slice(1), { stdio: ["ignore", "pipe", "inherit"] });
let buf = "";
race.stdout.on("data", (d) => {
  buf += d;
  let i;
  while ((i = buf.indexOf("\n")) >= 0) {
    const line = buf.slice(0, i);
    buf = buf.slice(i + 1);
    log.write(`${Date.now()}\t${line}\n`);
    process.stdout.write(line + "\n");
  }
});
race.on("exit", async (code) => {
  stop.done = true;
  await Promise.all(loops);
  log.end();
  process.exit(code ?? 0);
});
