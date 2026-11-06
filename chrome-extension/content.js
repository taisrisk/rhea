// Rhea page engine. Injected on demand by background.js; answers one
// command at a time. Elements are targeted by visible label (fuzzy: case,
// symbols and extra words don't matter; best visible match wins), by a
// controlId from the latest snapshot, or by CSS selector.
(() => {
  if (globalThis.__rheaPageEngine) return;
  globalThis.__rheaPageEngine = true;

  const INTERACTIVE = [
    "a[href]", "button", "input:not([type=hidden])", "select", "textarea", "summary", "label",
    "[role=button]", "[role=link]", "[role=tab]", "[role=menuitem]", "[role=menuitemcheckbox]", "[role=option]",
    "[role=checkbox]", "[role=radio]", "[role=switch]", "[role=combobox]", "[role=treeitem]", "[role=textbox]",
    "[role=searchbox]", "[contenteditable=''], [contenteditable=true]", "[onclick]", "[tabindex]:not([tabindex='-1'])",
  ].join(",");
  const TEXT_INPUT_TYPES = new Set(["", "text", "search", "email", "url", "tel", "number", "password"]);
  const ids = new WeakMap();
  const byId = new Map();
  let nextId = 1;

  // ---- matching ---------------------------------------------------------

  const words = (text) => String(text || "").toLowerCase().normalize("NFKC").split(/[^\p{L}\p{N}]+/u).filter(Boolean);

  function wordsScore(query, label) {
    const q = words(query), l = words(label);
    if (!q.length || !l.length) return 0;
    if (q.join(" ") === l.join(" ")) return 1000;
    let weakest = 3; const positions = [];
    for (const word of q) {
      let best = 0, at = -1;
      l.forEach((candidate, index) => {
        const tier = candidate === word ? 3 : candidate.startsWith(word) ? 2 : candidate.includes(word) ? 1 : 0;
        if (tier > best) { best = tier; at = index; }
      });
      if (!best) return 0;
      weakest = Math.min(weakest, best); positions.push(at);
    }
    const base = weakest === 3 ? 800 : weakest === 2 ? 600 : 400;
    const inOrder = positions.every((p, i) => i === 0 || p === positions[i - 1] + 1);
    return Math.max(1, base + (inOrder ? 50 : 0) - Math.min((l.length - q.length) * 25, base - 100));
  }

  // Score the whole label and each comma/bracket separated part of it.
  function score(query, label) {
    const whole = wordsScore(query, label);
    const parts = String(label || "").split(/[,()|·•\n\[\]]/).filter((part) => words(part).length);
    if (parts.length < 2) return whole;
    const penalty = 10 * (parts.length - 1);
    return parts.reduce((best, part) => { const s = wordsScore(query, part); return s ? Math.max(best, s - penalty) : best; }, whole);
  }

  // ---- element facts ----------------------------------------------------

  function isVisible(element) {
    if (!element.isConnected) return false;
    const rect = element.getBoundingClientRect();
    if (rect.width < 2 || rect.height < 2) return false;
    const style = getComputedStyle(element);
    return style.visibility !== "hidden" && style.display !== "none" && Number(style.opacity) > 0.02;
  }

  function inViewport(element) {
    const rect = element.getBoundingClientRect();
    return rect.bottom > 0 && rect.right > 0 && rect.top < innerHeight && rect.left < innerWidth;
  }

  function labelOf(element) {
    const attr = (name) => (element.getAttribute(name) || "").trim();
    let label = attr("aria-label");
    if (!label && attr("aria-labelledby")) {
      label = attr("aria-labelledby").split(/\s+/).map((id) => document.getElementById(id)?.innerText || "").join(" ").trim();
    }
    if (!label && element.labels?.length) label = Array.from(element.labels).map((l) => l.innerText || "").join(" ").trim();
    if (!label) label = attr("placeholder") || attr("title") || attr("alt") || attr("data-placeholder");
    if (!label && element.tagName === "INPUT" && ["button", "submit", "reset"].includes(element.type)) label = element.value;
    if (!label) label = (element.innerText || element.textContent || "").replace(/\s+/g, " ").trim();
    if (!label && element.querySelector) label = element.querySelector("img[alt]")?.alt || element.querySelector("svg title")?.textContent || "";
    return String(label || "").replace(/\s+/g, " ").trim().slice(0, 200);
  }

  function roleOf(element) {
    const explicit = element.getAttribute("role");
    if (explicit) return explicit;
    const tag = element.tagName.toLowerCase();
    if (tag === "a") return "link";
    if (tag === "button" || tag === "summary") return "button";
    if (tag === "select") return "combobox";
    if (tag === "textarea") return "textbox";
    if (tag === "input") {
      const type = (element.type || "text").toLowerCase();
      if (["button", "submit", "reset", "image"].includes(type)) return "button";
      if (type === "checkbox" || type === "radio") return type;
      return "textbox";
    }
    if (element.isContentEditable) return "textbox";
    return tag;
  }

  const isTextInput = (element) => {
    const tag = element.tagName;
    if (tag === "TEXTAREA" || element.isContentEditable) return true;
    if (tag === "INPUT") return TEXT_INPUT_TYPES.has((element.type || "").toLowerCase());
    return ["textbox", "searchbox", "combobox"].includes(element.getAttribute("role") || "");
  };

  function idOf(element) {
    let id = ids.get(element);
    if (!id) { id = `c${nextId++}`; ids.set(element, id); }
    byId.set(id, new WeakRef(element));
    return id;
  }

  function describe(element) {
    const item = { id: idOf(element), role: roleOf(element), label: labelOf(element) };
    if (element.tagName === "A" && element.href) item.href = element.href.slice(0, 500);
    if (isTextInput(element) && element.type !== "password") {
      const value = element.isContentEditable ? element.innerText : element.value;
      if (value) item.value = String(value).slice(0, 200);
    }
    if (element.tagName === "SELECT") item.selected = element.selectedOptions?.[0]?.text?.trim() || "";
    if (element.disabled) item.disabled = true;
    if (!inViewport(element)) item.offscreen = true;
    return item;
  }

  function interactiveElements() {
    return Array.from(document.querySelectorAll(INTERACTIVE)).filter(isVisible);
  }

  // ---- resolving a target -------------------------------------------------

  function resolve(params, kind) {
    if (params.controlId) {
      const element = byId.get(params.controlId)?.deref();
      if (element && isVisible(element)) return element;
    }
    if (params.selector) {
      const element = Array.from(document.querySelectorAll(params.selector)).find(isVisible);
      if (element) return element;
      throw new Error(`Nothing visible matches selector ${params.selector}`);
    }
    const target = params.target;
    if (!target) {
      if (kind === "input" && document.activeElement && isTextInput(document.activeElement)) return document.activeElement;
      if (kind === "input") {
        const fields = interactiveElements().filter(isTextInput);
        if (fields.length === 1) return fields[0];
      }
      throw new Error("Give a target label, a controlId or a selector");
    }
    const within = params.within ? Array.from(document.querySelectorAll("*")).filter((node) =>
      ["navigation", "main", "region", "dialog", "menu", "listbox", "form", "search", "complementary"].includes(node.getAttribute?.("role") || "")
      || ["NAV", "MAIN", "ASIDE", "FORM", "HEADER", "FOOTER", "DIALOG"].includes(node.tagName)).filter((area) => score(params.within, labelOf(area) || area.id || area.className) > 0) : null;
    const inArea = (element) => !within || within.some((area) => area.contains(element));
    let best = null, bestScore = 0;
    const consider = (element, label, bonus) => {
      if (!inArea(element)) return;
      if (kind === "input" && !isTextInput(element)) return;
      if (kind === "select" && !(element.tagName === "SELECT" || ["combobox", "listbox", "button"].includes(roleOf(element)))) return;
      const s = score(target, label);
      if (!s) return;
      const total = s * 10 + bonus + (inViewport(element) ? 40 : 0);
      if (total > bestScore) { best = element; bestScore = total; }
    };
    for (const element of interactiveElements()) consider(element, labelOf(element), 5);
    if (!best && kind !== "input") {
      // Plain text (e.g. a card title): click its nearest interactive ancestor, or itself.
      const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_ELEMENT);
      let count = 0;
      while (walker.nextNode() && count < 6000) {
        const element = walker.currentNode; count++;
        if (element.children.length > 3 || !isVisible(element)) continue;
        const text = (element.innerText || "").trim();
        if (!text || text.length > 200) continue;
        consider(element.closest(INTERACTIVE) || element, text, 0);
      }
    }
    if (!best) throw new Error(`Nothing visible matches '${target}'`);
    return best;
  }

  // ---- actions ------------------------------------------------------------

  function point(element) {
    element.scrollIntoView({ block: "center", inline: "center", behavior: "instant" });
    const rect = element.getBoundingClientRect();
    return { clientX: rect.left + rect.width / 2, clientY: rect.top + rect.height / 2 };
  }

  function mouse(element, type, at, extra = {}) {
    const Ctor = type.startsWith("pointer") ? PointerEvent : MouseEvent;
    element.dispatchEvent(new Ctor(type, { bubbles: true, cancelable: true, composed: true, view: window, button: 0, buttons: type.endsWith("down") ? 1 : 0, pointerType: "mouse", isPrimary: true, ...at, ...extra }));
  }

  function click(element, { double = false, right = false } = {}) {
    const at = point(element);
    const target = document.elementFromPoint(at.clientX, at.clientY);
    const hit = target && (element === target || element.contains(target)) ? target : element;
    const href = element.closest("a[href]")?.href;
    mouse(hit, "pointerover", at); mouse(hit, "mouseover", at);
    if (right) { mouse(hit, "pointerdown", at, { button: 2 }); mouse(hit, "mousedown", at, { button: 2 }); mouse(hit, "pointerup", at, { button: 2 }); mouse(hit, "mouseup", at, { button: 2 }); mouse(hit, "contextmenu", at, { button: 2 }); return {}; }
    for (let i = 0; i < (double ? 2 : 1); i++) {
      mouse(hit, "pointerdown", at); mouse(hit, "mousedown", at);
      if (typeof hit.focus === "function") hit.focus({ preventScroll: true });
      mouse(hit, "pointerup", at); mouse(hit, "mouseup", at); mouse(hit, "click", at, { detail: i + 1 });
    }
    if (double) mouse(hit, "dblclick", at, { detail: 2 });
    return { navigating: Boolean(href && !href.startsWith("javascript:") && new URL(href, location.href).href.split("#")[0] !== location.href.split("#")[0]) };
  }

  const isConsequentialAction = (value) => globalThis.RheaActionPolicy?.isConsequentialAction(value) === true;

  function submitterFor(element) {
    const form = element?.form || element?.closest?.("form");
    if (!form) return null;
    return Array.from(form.querySelectorAll("button, input[type=submit], [role=button]")).find((candidate) =>
      isConsequentialAction(labelOf(candidate) || candidate.value || "")) || null;
  }

  function consequentialControl(element) {
    if (!element) return null;
    const label = (labelOf(element) || "").slice(0, 180);
    const link = element.closest?.("a[href]");
    const destination = link?.getAttribute("href") || "";
    if (isConsequentialAction(`${label} ${destination}`)) return { label: label || destination, element };
    const submitter = submitterFor(element);
    if (submitter) return { label: (labelOf(submitter) || "Submit form").slice(0, 180), element: submitter };
    return null;
  }

  function requestUserApproval(action, detail) {
    return new Promise((resolve) => {
      const host = document.createElement("div");
      host.setAttribute("data-rhea-approval", "true");
      host.style.cssText = "all:initial;position:fixed;inset:0;z-index:2147483647;pointer-events:auto";
      const shadow = host.attachShadow({ mode: "closed" });
      const style = document.createElement("style");
      style.textContent = `
        :host { all: initial; }
        .shade { position:fixed; inset:0; display:grid; place-items:center; background:rgba(4,7,6,.72); font:14px/1.45 system-ui,sans-serif; color:#f5f6f5; }
        .dialog { width:min(440px,calc(100vw - 36px)); box-sizing:border-box; padding:22px; border:1px solid #3b4941; border-radius:16px; background:#151a17; box-shadow:0 18px 70px rgba(0,0,0,.62),0 0 34px rgba(47,156,104,.16); }
        h2 { margin:0 0 9px; font-size:17px; font-weight:650; }
        p { margin:0; color:#d1d8d3; overflow-wrap:anywhere; }
        .detail { margin-top:9px; color:#aeb9b2; font-size:12px; }
        .host { margin-top:12px; color:#7dbd99; font-size:12px; }
        .actions { display:flex; justify-content:flex-end; gap:9px; margin-top:20px; }
        button { border:1px solid #3b4941; border-radius:9px; padding:9px 13px; font:600 13px system-ui,sans-serif; cursor:pointer; }
        .deny { color:#f2f4f2; background:#232a25; }
        .allow { color:#06100a; background:#e9f1ec; }
        button:focus-visible { outline:2px solid #69ce98; outline-offset:2px; }
      `;
      const shade = document.createElement("div");
      shade.className = "shade";
      const dialog = document.createElement("section");
      dialog.className = "dialog";
      dialog.setAttribute("role", "alertdialog");
      dialog.setAttribute("aria-modal", "true");
      const title = document.createElement("h2");
      title.textContent = "Rhea approval required";
      const description = document.createElement("p");
      description.textContent = `Allow the browser action: ${action}?`;
      const extra = document.createElement("p");
      extra.className = "detail";
      extra.textContent = detail ? String(detail).slice(0, 180) : "";
      const origin = document.createElement("p");
      origin.className = "host";
      origin.textContent = `Website: ${location.hostname || location.origin}`;
      const actions = document.createElement("div");
      actions.className = "actions";
      const deny = document.createElement("button");
      deny.type = "button";
      deny.className = "deny";
      deny.textContent = "Block";
      const allow = document.createElement("button");
      allow.type = "button";
      allow.className = "allow";
      allow.textContent = "Allow once";
      actions.append(deny, allow);
      dialog.append(title, description, extra, origin, actions);
      shade.append(dialog);
      shadow.append(style, shade);
      document.documentElement.append(host);

      let settled = false;
      const finish = (approved) => {
        if (settled) return;
        settled = true;
        clearTimeout(timeout);
        host.remove();
        resolve(approved);
      };
      const timeout = setTimeout(() => finish(false), 120000);
      deny.addEventListener("click", () => finish(false), { once: true });
      allow.addEventListener("click", () => finish(true), { once: true });
      shade.addEventListener("click", (event) => {
        if (event.target === shade) finish(false);
      });
      deny.focus();
    });
  }

  async function confirmConsequentialAction(element, detail = "") {
    const item = consequentialControl(element);
    if (!item) return;
    const approved = await requestUserApproval(item.label, detail);
    if (!approved) throw new Error(`User blocked the consequential browser action '${item.label}'.`);
    if (!item.element.isConnected || !isVisible(item.element)) {
      throw new Error("The approved browser control changed or disappeared; inspect a fresh snapshot before retrying.");
    }
  }

  function setNativeValue(element, value) {
    const proto = element.tagName === "TEXTAREA" ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
    Object.getOwnPropertyDescriptor(proto, "value")?.set?.call(element, value);
  }

  function key(element, name, extra = {}) {
    const codes = { Enter: 13, Escape: 27, Tab: 9, Backspace: 8, Delete: 46, ArrowUp: 38, ArrowDown: 40, ArrowLeft: 37, ArrowRight: 39, " ": 32, Home: 36, End: 35, PageUp: 33, PageDown: 34 };
    const init = { key: name, code: name.length === 1 ? `Key${name.toUpperCase()}` : name, keyCode: codes[name] || name.toUpperCase().charCodeAt(0), which: codes[name] || 0, bubbles: true, cancelable: true, composed: true, ...extra };
    const down = element.dispatchEvent(new KeyboardEvent("keydown", init));
    if (name.length === 1 || name === "Enter") element.dispatchEvent(new KeyboardEvent("keypress", init));
    element.dispatchEvent(new KeyboardEvent("keyup", init));
    return down; // false when the page handled (prevented) it
  }

  function submitFrom(element) {
    const handled = !key(element, "Enter");
    const form = element.form || element.closest?.("form");
    if (!handled && form && element.tagName !== "TEXTAREA") {
      form.requestSubmit ? form.requestSubmit() : form.submit();
      return true;
    }
    return handled || Boolean(form);
  }

  function type(element, text, { clear = true, submit = false } = {}) {
    point(element);
    element.focus({ preventScroll: true });
    if (element.isContentEditable) {
      const selection = getSelection();
      selection.removeAllRanges();
      const range = document.createRange();
      range.selectNodeContents(element);
      if (!clear) range.collapse(false);
      selection.addRange(range);
      // insertText goes through the editor's own input pipeline (Slate, Lexical, ProseMirror, Draft).
      if (!document.execCommand("insertText", false, text)) {
        element.textContent = clear ? text : element.textContent + text;
        element.dispatchEvent(new InputEvent("input", { bubbles: true, inputType: "insertText", data: text }));
      }
    } else {
      setNativeValue(element, clear ? text : `${element.value || ""}${text}`);
      element.dispatchEvent(new InputEvent("input", { bubbles: true, inputType: "insertText", data: text }));
      element.dispatchEvent(new Event("change", { bubbles: true }));
    }
    let navigating = false;
    if (submit) navigating = submitFrom(element);
    const value = element.isContentEditable ? element.innerText : element.value;
    return { typed: text.length, confirmed: String(value || "").includes(text.slice(0, 40)), navigating };
  }

  async function select(element, option) {
    await confirmConsequentialAction(element);
    if (element.tagName === "SELECT") {
      let best = null, bestScore = 0;
      for (const item of element.options) {
        const s = Math.max(score(option, item.text), score(option, item.value) - 50);
        if (s > bestScore) { best = item; bestScore = s; }
      }
      if (!best) throw new Error(`No option matches '${option}'. Options: ${Array.from(element.options).map((o) => o.text.trim()).slice(0, 30).join(" | ")}`);
      element.value = best.value;
      element.dispatchEvent(new Event("input", { bubbles: true }));
      element.dispatchEvent(new Event("change", { bubbles: true }));
      return { selected: best.text.trim() };
    }
    // Custom dropdown: open it, then click the best visible option.
    click(element);
    for (let attempt = 0; attempt < 20; attempt++) {
      await new Promise((r) => setTimeout(r, 60));
      const options = Array.from(document.querySelectorAll("[role=option],[role=menuitem],[role=menuitemradio],li,[data-value]")).filter(isVisible);
      let best = null, bestScore = 0;
      for (const item of options) { const s = score(option, labelOf(item)); if (s > bestScore) { best = item; bestScore = s; } }
      if (best) { await confirmConsequentialAction(best); click(best); return { selected: labelOf(best) }; }
    }
    throw new Error(`The dropdown opened but no option matches '${option}'`);
  }

  function press(keys, element) {
    const parts = String(keys || "").split("+").map((part) => part.trim()).filter(Boolean);
    const main = parts.pop() || "";
    const names = { enter: "Enter", return: "Enter", esc: "Escape", escape: "Escape", tab: "Tab", space: " ", backspace: "Backspace", delete: "Delete",
      up: "ArrowUp", down: "ArrowDown", left: "ArrowLeft", right: "ArrowRight", home: "Home", end: "End", pageup: "PageUp", pagedown: "PageDown" };
    const name = names[main.toLowerCase()] || (main.length === 1 ? main : main);
    const mods = { ctrlKey: parts.some((p) => /^(ctrl|control)$/i.test(p)), shiftKey: parts.some((p) => /^shift$/i.test(p)), altKey: parts.some((p) => /^alt$/i.test(p)), metaKey: parts.some((p) => /^(meta|win|cmd)$/i.test(p)) };
    const target = element || document.activeElement || document.body;
    if (name === "Enter" && !mods.ctrlKey && !mods.altKey && !mods.shiftKey) return { navigating: submitFrom(target) };
    if (name === "Tab" && !mods.ctrlKey) {
      key(target, "Tab", mods);
      const focusable = interactiveElements().filter((el) => !el.disabled);
      const index = focusable.indexOf(target);
      const next = focusable[(index + (mods.shiftKey ? -1 : 1) + focusable.length) % focusable.length];
      next?.focus();
      return { focused: next ? labelOf(next) : "" };
    }
    if (name.startsWith("Page") || name === "Home" || name === "End") {
      const amount = name === "PageDown" ? innerHeight * 0.85 : name === "PageUp" ? -innerHeight * 0.85 : 0;
      key(target, name, mods);
      if (name === "Home") scrollTo({ top: 0 }); else if (name === "End") scrollTo({ top: document.body.scrollHeight }); else scrollBy({ top: amount });
      return {};
    }
    key(target, name, mods);
    return {};
  }

  function extract(params) {
    const limit = Math.min(Number(params.limit) || 200, 1000);
    const kind = params.kind || "text";
    const root = params.selector ? document.querySelector(params.selector) : document.body;
    if (!root) throw new Error(`Nothing matches selector ${params.selector}`);
    if (kind === "text") return { text: (root.innerText || "").replace(/\n{3,}/g, "\n\n").slice(0, Math.min(Number(params.max_chars) || 30000, 100000)) };
    if (kind === "links") return { links: Array.from(root.querySelectorAll("a[href]")).filter(isVisible).slice(0, limit).map((a) => ({ text: labelOf(a), href: a.href })) };
    if (kind === "tables") return { tables: Array.from(root.querySelectorAll("table")).slice(0, 20).map((table) => Array.from(table.rows).slice(0, limit).map((row) => Array.from(row.cells).map((cell) => cell.innerText.trim().slice(0, 300)))) };
    if (kind === "headings") return { headings: Array.from(root.querySelectorAll("h1,h2,h3,h4,[role=heading]")).filter(isVisible).slice(0, limit).map((h) => ({ level: Number(h.tagName[1]) || Number(h.getAttribute("aria-level")) || 0, text: labelOf(h) })) };
    if (kind === "images") return { images: Array.from(root.querySelectorAll("img")).filter(isVisible).slice(0, limit).map((img) => ({ alt: img.alt, src: img.currentSrc || img.src })) };
    if (kind === "forms") return { fields: Array.from(root.querySelectorAll("input:not([type=hidden]),select,textarea")).filter(isVisible).slice(0, limit).map((f) => ({ id: idOf(f), label: labelOf(f), type: f.type || f.tagName.toLowerCase(), value: f.type === "password" ? undefined : String(f.value || "").slice(0, 200) })) };
    if (kind === "elements") return { elements: Array.from(root.querySelectorAll(params.match || "*")).filter(isVisible).slice(0, limit).map((el) => ({ id: idOf(el), tag: el.tagName.toLowerCase(), text: labelOf(el) })) };
    throw new Error(`Unknown extract kind '${kind}' (text, links, tables, headings, images, forms, elements)`);
  }

  async function wait(params) {
    const timeout = Math.min(Number(params.timeout_ms) || 8000, 30000);
    const deadline = Date.now() + timeout;
    const present = () => {
      if (params.selector) return Array.from(document.querySelectorAll(params.selector)).some(isVisible);
      if (params.text) return words(document.body?.innerText || "").join(" ").includes(words(params.text).join(" "));
      return document.readyState === "complete";
    };
    while (Date.now() < deadline) {
      if (Boolean(present()) !== Boolean(params.gone)) return { waited_ms: timeout - (deadline - Date.now()) };
      await new Promise((r) => setTimeout(r, 100));
    }
    throw new Error(params.gone ? "It did not disappear in time" : `'${params.text || params.selector || "page"}' did not appear in time`);
  }

  function snapshot(params) {
    const limit = Math.min(Number(params.limit) || 150, 400);
    let elements = interactiveElements();
    if (params.target) {
      elements = elements.map((el) => [el, score(params.target, labelOf(el))]).filter(([, s]) => s > 0).sort((a, b) => b[1] - a[1]).map(([el]) => el);
    } else {
      elements.sort((a, b) => Number(inViewport(b)) - Number(inViewport(a)));
    }
    const controls = elements.filter((el) => labelOf(el) || isTextInput(el)).slice(0, limit).map(describe);
    return {
      url: location.href, title: document.title,
      text: (document.body?.innerText || "").replace(/\n{3,}/g, "\n\n").slice(0, Math.min(Number(params.max_chars) || 4000, 20000)),
      controls,
      scroll: { y: Math.round(scrollY), height: document.body?.scrollHeight || 0, viewport: innerHeight },
    };
  }

  async function run(method, params) {
    switch (method) {
      case "snapshot": return snapshot(params);
      case "click": { const el = resolve(params, "click"); if (!params.right) await confirmConsequentialAction(el); return { clicked: labelOf(el), role: roleOf(el), ...click(el, { double: params.double, right: params.right }) }; }
      case "hover": { const el = resolve(params, "click"); const at = point(el); mouse(el, "pointerover", at); mouse(el, "mouseover", at); mouse(el, "mousemove", at); return { hovered: labelOf(el) }; }
      case "type": {
        const el = resolve(params, "input");
        const typed = type(el, String(params.text ?? ""), { clear: params.clear !== false, submit: false });
        let navigating = false;
        if (params.submit === true) {
          await confirmConsequentialAction(el, `Submit text: ${String(params.text ?? "").slice(0, 120)}`);
          navigating = submitFrom(el);
        }
        return { field: labelOf(el), ...typed, navigating };
      }
      case "select": { const el = resolve(params, "select"); return { field: labelOf(el), ...(await select(el, String(params.option ?? params.text ?? ""))) }; }
      case "press": {
        const target = params.target || params.controlId || params.selector ? resolve(params, "any") : document.activeElement;
        const pressesEnter = String(params.keys || "").toLowerCase().split("+").some((part) => ["enter", "return"].includes(part.trim()));
        if (pressesEnter) await confirmConsequentialAction(target);
        return press(params.keys, target);
      }
      case "scroll": {
        if (params.target || params.controlId || params.selector) { const el = resolve(params, "any"); point(el); return { scrolledTo: labelOf(el) }; }
        const amount = Number(params.amount) || 3;
        scrollBy({ top: (params.direction === "up" ? -1 : 1) * amount * innerHeight * 0.33, behavior: "instant" });
        return { y: Math.round(scrollY) };
      }
      case "extract": return extract(params);
      case "wait": return wait(params);
    }
    throw new Error(`Unknown page method ${method}`);
  }

  chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
    if (sender.id !== chrome.runtime.id || sender.tab || !message?.rhea) return false;
    run(message.method, message.params || {}).then(
      (value) => sendResponse({ ok: true, value, navigating: Boolean(value?.navigating) }),
      (error) => sendResponse({ ok: false, error: String(error?.message || error) }),
    );
    return true; // async response
  });
})();
