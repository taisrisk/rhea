// Rhea Browser Bridge: connects to the local Rhea native host as soon as
// Chrome runs and stays connected, so Rhea can work with any tab without a
// popup click. Tab-level commands run here; page-level commands run in the
// content script (content.js), which is injected on demand.

const NATIVE_HOST = "com.rhea.browser";
const PAGE_METHODS = new Set(["snapshot", "click", "type", "select", "press", "scroll", "extract", "wait", "hover"]);
const TAB_METHODS = new Set(["ping", "tabs", "activate", "open", "close", "navigate", "back", "forward", "reload"]);
const KEEPALIVE_ALARM = "rhea-keepalive";

let nativePort = null;
let connectedAt = null;
let lastError = "";
let reconnectDelay = 1000;
let reconnectTimer = null;

function status() {
  return { connected: nativePort !== null, host: NATIVE_HOST, connectedAt, lastError };
}

function broadcastStatus() {
  chrome.runtime.sendMessage({ type: "rhea-status", status: status() }).catch(() => {});
}

function scheduleReconnect() {
  clearTimeout(reconnectTimer);
  reconnectTimer = setTimeout(connectNative, reconnectDelay);
  reconnectDelay = Math.min(reconnectDelay * 2, 30_000);
}

function connectNative() {
  if (nativePort) return status();
  try {
    const port = chrome.runtime.connectNative(NATIVE_HOST);
    nativePort = port;
    connectedAt = new Date().toISOString();
    lastError = "";
    port.onMessage.addListener((message) => {
      handleNativeMessage(message).catch((error) => {
        const id = typeof message?.id === "string" ? message.id : "";
        post({ type: "command_result", id, ok: false, error: String(error?.message || error) });
      });
    });
    port.onDisconnect.addListener(() => {
      lastError = chrome.runtime.lastError?.message || "Rhea native host disconnected";
      nativePort = null;
      connectedAt = null;
      broadcastStatus();
      scheduleReconnect();
    });
    post({ type: "ready", protocol: 2 });
    reconnectDelay = 1000;
    broadcastStatus();
  } catch (error) {
    nativePort = null;
    lastError = String(error?.message || error);
    broadcastStatus();
    scheduleReconnect();
  }
  return status();
}

function post(message) {
  if (!nativePort) throw new Error("Rhea native host is disconnected");
  nativePort.postMessage(message);
}

// ---- tabs ---------------------------------------------------------------

function words(text) {
  return String(text || "").toLowerCase().normalize("NFKC").split(/[^\p{L}\p{N}]+/u).filter(Boolean);
}

function fuzzy(query, label) {
  const q = words(query), l = words(label);
  if (!q.length || !l.length) return 0;
  if (q.join(" ") === l.join(" ")) return 1000;
  let weakest = 3;
  for (const word of q) {
    let tier = 0;
    for (const candidate of l) {
      tier = Math.max(tier, candidate === word ? 3 : candidate.startsWith(word) ? 2 : candidate.includes(word) ? 1 : 0);
    }
    if (!tier) return 0;
    weakest = Math.min(weakest, tier);
  }
  const base = weakest === 3 ? 800 : weakest === 2 ? 600 : 400;
  return Math.max(1, base - Math.min((l.length - q.length) * 12, base - 100));
}

function describeTab(tab) {
  return { tabId: tab.id, windowId: tab.windowId, title: tab.title || "", url: tab.url || "", active: tab.active, status: tab.status };
}

async function resolveTab(params) {
  if (Number.isInteger(params.tabId)) return chrome.tabs.get(params.tabId);
  if (typeof params.tab === "string" && params.tab.trim()) {
    const tabs = await chrome.tabs.query({});
    let best = null, bestScore = 0;
    for (const tab of tabs) {
      const score = Math.max(fuzzy(params.tab, tab.title), fuzzy(params.tab, tab.url) * 0.9);
      if (score > bestScore) { best = tab; bestScore = score; }
    }
    if (!best) throw new Error(`No open tab matches '${params.tab}'`);
    return best;
  }
  const [active] = await chrome.tabs.query({ active: true, lastFocusedWindow: true });
  if (active) return active;
  const [any] = await chrome.tabs.query({ active: true });
  if (!any) throw new Error("No Chrome tab is open");
  return any;
}

function waitForLoad(tabId, timeoutMs = 15_000) {
  return new Promise((resolve) => {
    let done = false;
    const finish = () => { if (!done) { done = true; chrome.tabs.onUpdated.removeListener(listener); clearTimeout(timer); resolve(); } };
    const listener = (id, info) => { if (id === tabId && info.status === "complete") finish(); };
    const timer = setTimeout(finish, timeoutMs);
    chrome.tabs.onUpdated.addListener(listener);
    chrome.tabs.get(tabId).then((tab) => { if (tab.status === "complete") setTimeout(finish, 50); }).catch(finish);
  });
}

function normalizeUrl(url) {
  if (typeof url !== "string" || !url.trim()) throw new Error("A URL is required");
  let text = url.trim();
  if (!/^[a-z][a-z0-9+.-]*:/i.test(text)) {
    // "youtube.com" -> https://youtube.com ; plain words -> a Google search
    text = /\s/.test(text) || !text.includes(".") ? `https://www.google.com/search?q=${encodeURIComponent(text)}` : `https://${text}`;
  }
  const parsed = new URL(text);
  if (!["http:", "https:"].includes(parsed.protocol)) throw new Error("Only http and https pages can be opened");
  return parsed.href;
}

async function runTabMethod(method, params) {
  switch (method) {
    case "ping": {
      const tabs = await chrome.tabs.query({});
      return { ok: true, version: chrome.runtime.getManifest().version, tabs: tabs.length };
    }
    case "tabs": {
      const tabs = await chrome.tabs.query({});
      const [focused] = await chrome.tabs.query({ active: true, lastFocusedWindow: true });
      return { tabs: tabs.map(describeTab), focusedTabId: focused?.id ?? null };
    }
    case "activate": {
      const tab = await resolveTab(params);
      await chrome.tabs.update(tab.id, { active: true });
      await chrome.windows.update(tab.windowId, { focused: true });
      return { tab: describeTab(await chrome.tabs.get(tab.id)) };
    }
    case "open": {
      const tab = await chrome.tabs.create({ url: normalizeUrl(params.url), active: params.background !== true });
      await waitForLoad(tab.id);
      return { tab: describeTab(await chrome.tabs.get(tab.id)) };
    }
    case "close": {
      const tab = await resolveTab(params);
      await chrome.tabs.remove(tab.id);
      return { closed: tab.id };
    }
    case "navigate": {
      const tab = await resolveTab(params);
      await chrome.tabs.update(tab.id, { url: normalizeUrl(params.url) });
      await waitForLoad(tab.id);
      return { tab: describeTab(await chrome.tabs.get(tab.id)) };
    }
    case "back":
    case "forward":
    case "reload": {
      const tab = await resolveTab(params);
      if (method === "back") await chrome.tabs.goBack(tab.id);
      else if (method === "forward") await chrome.tabs.goForward(tab.id);
      else await chrome.tabs.reload(tab.id);
      await waitForLoad(tab.id);
      return { tab: describeTab(await chrome.tabs.get(tab.id)) };
    }
  }
  throw new Error(`Unknown tab method ${method}`);
}

async function runPageMethod(method, params) {
  const tab = await resolveTab(params);
  if (!/^https?:/.test(tab.url || "")) throw new Error(`Tab '${tab.title}' is a browser page Rhea cannot read (${(tab.url || "").split(":")[0]})`);
  const send = () => chrome.tabs.sendMessage(tab.id, { rhea: true, method, params });
  let result;
  try {
    result = await send();
  } catch {
    // Not injected yet (or the page navigated): inject, then retry once.
    await chrome.scripting.executeScript({ target: { tabId: tab.id }, files: ["action-policy.js", "content.js"], world: "ISOLATED" });
    result = await send();
  }
  if (!result?.ok) throw new Error(result?.error || "The page did not complete the action");
  // A click or Enter can start a navigation; let it settle before returning.
  if (["click", "press", "type"].includes(method) && result.navigating) await waitForLoad(tab.id, 10_000);
  const latest = await chrome.tabs.get(tab.id).catch(() => tab);
  return { ...result.value, tab: describeTab(latest) };
}

async function handleNativeMessage(message) {
  if (message?.type === "shutdown") {
    const port = nativePort;
    setTimeout(() => { if (nativePort === port) port.disconnect(); }, 100);
    return;
  }
  if (message?.type !== "command") return;
  const id = typeof message.id === "string" && message.id.length <= 128 ? message.id : "";
  const method = String(message.method || "");
  const params = message.params && typeof message.params === "object" && !Array.isArray(message.params) ? message.params : {};
  if (!id) return;
  try {
    let result;
    if (TAB_METHODS.has(method)) result = await runTabMethod(method, params);
    else if (PAGE_METHODS.has(method)) result = await runPageMethod(method, params);
    else throw new Error(`Unsupported browser method '${method}'`);
    post({ type: "command_result", id, ok: true, result });
  } catch (error) {
    post({ type: "command_result", id, ok: false, error: String(error?.message || error) });
  }
}

// ---- lifetime -------------------------------------------------------------

chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  if (sender.id !== chrome.runtime.id || sender.tab) return false;
  if (message?.type === "status") { sendResponse({ ok: true, status: status() }); return false; }
  if (message?.type === "connect") { reconnectDelay = 1000; sendResponse({ ok: true, status: connectNative() }); return false; }
  return false;
});

chrome.alarms.create(KEEPALIVE_ALARM, { periodInMinutes: 0.5 });
chrome.alarms.onAlarm.addListener((alarm) => {
  if (alarm.name !== KEEPALIVE_ALARM) return;
  if (!nativePort) connectNative();
  else { try { post({ type: "heartbeat", at: Date.now() }); } catch { /* onDisconnect reconnects */ } }
});
chrome.runtime.onStartup.addListener(connectNative);
chrome.runtime.onInstalled.addListener(connectNative);
connectNative();
