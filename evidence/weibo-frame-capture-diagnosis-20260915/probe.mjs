#!/usr/bin/env node
import { execFileSync, spawn } from "node:child_process";
import { createConnection } from "node:net";
import { createHash } from "node:crypto";
import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const root = process.cwd();
const hostBin = process.env.OBSCURA_HOST_BIN || join(root, "target/debug/obscura-host");
const url = process.env.WEIBO_URL || "https://weibo.com";
const commit = process.env.OBSCURA_COMMIT || execFileSync("git", ["rev-parse", "HEAD"], { cwd: root, encoding: "utf8" }).trim();
const evidenceDir = join(root, "evidence/weibo-frame-capture-diagnosis-20260915");
mkdirSync(evidenceDir, { recursive: true });

const socketDirParent = mkdtempSync(join(tmpdir(), "weibo-frame-probe-socket-"));
const socketDir = join(socketDirParent, "socket");
const storageDir = mkdtempSync(join(tmpdir(), "weibo-frame-probe-profile-"));
const host = spawn(hostBin, ["--socket-dir", socketDir, "--storage-dir", storageDir, "--allow-private-network"], {
  stdio: ["ignore", "pipe", "pipe"],
});
const hostLogPath = join(evidenceDir, "host.log");
const hostLog = [];
host.stdout.on("data", (chunk) => hostLog.push(chunk.toString()));
host.stderr.on("data", (chunk) => hostLog.push(chunk.toString()));
host.once("error", (error) => console.error(`host spawn error: ${error}`));
host.once("exit", (code, signal) => console.error(`host exited: code=${code} signal=${signal}`));

function waitForSocket(path, timeoutMs = 30000) {
  const start = Date.now();
  return new Promise((resolve, reject) => {
    const timer = setInterval(() => {
      try {
        createConnection(path).once("connect", function () {
          this.destroy();
          clearInterval(timer);
          resolve();
        }).once("error", () => {});
      } catch {
        // not ready
      }
      if (Date.now() - start > timeoutMs) {
        clearInterval(timer);
        reject(new Error(`socket timeout: ${path}`));
      }
    }, 100);
  });
}

function connect(path) {
  return new Promise((resolve, reject) => {
    const socket = createConnection(path);
    socket.once("connect", () => resolve(socket));
    socket.once("error", reject);
  });
}

function readLine(socket) {
  return new Promise((resolve, reject) => {
    let buffer = Buffer.alloc(0);
    const onData = (chunk) => {
      buffer = Buffer.concat([buffer, chunk]);
      const nl = buffer.indexOf(10);
      if (nl === -1) return;
      socket.off("data", onData);
      resolve(buffer.subarray(0, nl).toString("utf8"));
      buffer = buffer.subarray(nl + 1);
    };
    socket.on("data", onData);
    socket.once("error", reject);
  });
}

function sendJson(socket, value) {
  socket.write(JSON.stringify(value) + "\n");
}

async function requestControl(control, id, command, operation = null) {
  sendJson(control, { id, command, operation });
  const line = await readLine(control);
  const response = JSON.parse(line);
  if (response.type === "error") {
    throw new Error(`${response.code}: ${response.message}`);
  }
  if (response.id !== id) {
    throw new Error(`response id mismatch: expected ${id}, got ${response.id}`);
  }
  return response;
}

async function run() {
  await waitForSocket(join(socketDir, "host.sock"));
  await waitForSocket(join(socketDir, "frames.sock"));

  const control = await connect(join(socketDir, "host.sock"));
  const frames = await connect(join(socketDir, "frames.sock"));

  const readyLine = await readLine(control);
  const ready = JSON.parse(readyLine);
  if (ready.type !== "ready") throw new Error(`expected ready: ${readyLine}`);

  const packets = [];
  let frameBytes = Buffer.alloc(0);
  let pendingFrame = null;
  let lastFramePixels = null;
  let lastFrameInfo = null;
  frames.on("data", (chunk) => {
    frameBytes = Buffer.concat([frameBytes, chunk]);
    for (;;) {
      if (pendingFrame) {
        if (frameBytes.length < pendingFrame.byte_length) return;
        const pixels = frameBytes.subarray(0, pendingFrame.byte_length);
        packets.push({
          type: "frame",
          info: pendingFrame.info,
          sha256: createHash("sha256").update(pixels).digest("hex"),
        });
        lastFramePixels = pixels;
        lastFrameInfo = pendingFrame.info;
        frameBytes = frameBytes.subarray(pendingFrame.byte_length);
        pendingFrame = null;
        continue;
      }
      const nl = frameBytes.indexOf(10);
      if (nl === -1) return;
      const header = JSON.parse(frameBytes.subarray(0, nl).toString("utf8"));
      frameBytes = frameBytes.subarray(nl + 1);
      if (header.type === "frame") {
        pendingFrame = { info: header.info, byte_length: Number(header.info.byte_length) };
      } else {
        packets.push(header);
      }
    }
  });

  const attach = await requestControl(control, 1, { type: "attach", mode: "agent", viewport: null });
  const status = attach.value;
  const sessionId = status.session_id;
  const attachmentId = status.attachment_id;
  const nav = await requestControl(control, 2, { type: "navigate", url }, {
    operation_id: "weibo-nav",
    session_id: sessionId,
    attachment_id: attachmentId,
    sequence: status.next_sequence,
    control_epoch: status.control.epoch,
    viewport_revision: status.viewport_revision,
    document_revision: status.document_revision,
  });

  const deadline = Date.now() + 60000;
  while (Date.now() < deadline) {
    const current = await requestControl(control, 3, { type: "status" });
    const finalDocumentRevision = Math.max(3, current.value.document_revision);
    const frameRevCount = packets.filter((p) => p.type === "frame").length;
    const unavailable = packets.filter((p) => p.type === "unavailable");
    if (frameRevCount > 0 && unavailable.length > 0 && current.value.document_revision >= finalDocumentRevision - 1) {
      break;
    }
    await new Promise((resolve) => setTimeout(resolve, 500));
  }

  const domExpression = `JSON.stringify({href:location.href,title:document.title,bodyRect:(()=>{const r=document.body.getBoundingClientRect();return {x:r.x,y:r.y,width:r.width,height:r.height}})(),scrollY:window.scrollY,docScrollTop:document.documentElement.scrollTop,docScrollHeight:document.documentElement.scrollHeight,innerHeight:innerHeight,appChildren:document.querySelector('#app')?.childElementCount??-1,inputs:document.querySelectorAll('input').length,buttons:document.querySelectorAll('button').length})`;
  const statusAfterNav = await requestControl(control, 3, { type: "status" });
  const evaluation = await requestControl(control, 4, { type: "evaluate", expression: domExpression }, {
    operation_id: "weibo-eval",
    session_id: sessionId,
    attachment_id: attachmentId,
    sequence: statusAfterNav.value.next_sequence,
    control_epoch: statusAfterNav.value.control.epoch,
    viewport_revision: statusAfterNav.value.viewport_revision,
    document_revision: statusAfterNav.value.document_revision,
  });
  await requestControl(control, 5, { type: "detach" });

  writeFileSync(join(evidenceDir, "probe.json"), JSON.stringify({
    commit,
    navigation: nav,
    packets,
    evaluation,
    status: statusAfterNav.value,
    source: { socketDir, storageDir },
  }, null, 2) + "\n");
  if (lastFramePixels && lastFrameInfo) {
    const rawPath = join(evidenceDir, "last-frame.raw");
    const pngPath = join(evidenceDir, "last-frame.png");
    writeFileSync(rawPath, lastFramePixels);
    const { width, height } = lastFrameInfo.source_dimensions;
    execFileSync("magick", [
      "convert",
      "-size",
      `${width}x${height}`,
      "-depth",
      "8",
      `RGBA:${rawPath}`,
      `PNG:${pngPath}`,
    ], { stdio: "ignore" });
  }
  control.destroy();
  frames.destroy();
}

async function main() {
  try {
    await run();
  } finally {
    host.kill("SIGTERM");
    await new Promise((resolve) => host.once("exit", resolve));
    rmSync(socketDirParent, { recursive: true, force: true });
    writeFileSync(hostLogPath, hostLog.join(""), "utf8");
  }
}

try {
  await main();
} catch (error) {
  console.error(error);
  process.exitCode = 1;
}
