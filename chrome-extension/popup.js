const dot = document.getElementById("dot");
const state = document.getElementById("state");
const error = document.getElementById("error");

function render(status) {
  const connected = Boolean(status?.connected);
  dot.className = `dot ${connected ? "on" : "off"}`;
  state.textContent = connected ? "Connected to Rhea" : "Not connected · Rhea reconnects automatically";
  error.hidden = connected || !status?.lastError;
  error.textContent = status?.lastError || "";
}

chrome.runtime.sendMessage({ type: "status" }).then((reply) => render(reply?.status)).catch(() => render(null));
chrome.runtime.onMessage.addListener((message) => { if (message?.type === "rhea-status") render(message.status); });
document.getElementById("reconnect").addEventListener("click", async () => {
  const reply = await chrome.runtime.sendMessage({ type: "connect" }).catch(() => null);
  render(reply?.status);
});
