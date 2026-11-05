const test = require("node:test");
const assert = require("node:assert/strict");
const { isConsequentialAction } = require("./action-policy.js");

test("consequential browser controls require a user approval prompt", () => {
  for (const label of ["Send message", "Delete", "Purchase now", "Submit", "Post", "Transfer funds"]) {
    assert.equal(isConsequentialAction(label), true, label);
  }
});

test("ordinary navigation and inspection controls remain unblocked", () => {
  for (const label of ["Search", "Open menu", "Continue reading", "Settings", "Next page"]) {
    assert.equal(isConsequentialAction(label), false, label);
  }
});
