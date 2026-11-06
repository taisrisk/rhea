(() => {
  const consequentialAction = /\b(send|submit|post|publish|delete|remove|purchase|buy|checkout|pay|transfer|confirm|unsubscribe|block|report|revoke|sign out|log out)\b/i;
  const api = Object.freeze({
    isConsequentialAction(value) {
      return consequentialAction.test(String(value || ""));
    },
  });
  Object.defineProperty(globalThis, "RheaActionPolicy", {
    value: api,
    configurable: true,
    writable: false,
  });
  if (typeof module !== "undefined" && module.exports) module.exports = api;
})();
