import assert from "node:assert/strict";
import test from "node:test";

import { formatProgress, parseTasks } from "./conversion-progress.mjs";

test("conversion progress keeps the checked-task output stable", () => {
  const stats = parseTasks("- [x] 1. first\n- [ ] S-002 second\n");
  assert.deepEqual(stats, { checked: 1, total: 2, open: 1 });
  assert.equal(formatProgress(stats), "Conversion progress: 50.00% (1/2; 1 open)");
});

for (const [name, fixture] of [
  ["malformed status", "- [?] 1. task\n"],
  ["malformed numeric id", "- [ ] 2 task\n"],
  ["malformed supplemental id", "- [ ] S-ABC task\n"],
  ["duplicate numeric id", "- [x] 1. first\n- [ ] 1. again\n"],
  ["duplicate supplemental id", "- [x] S-001 first\n- [ ] S-001 again\n"],
  ["empty task set", "# no tasks\n"],
]) {
  test(`conversion progress rejects ${name}`, () => {
    assert.throws(() => parseTasks(fixture), /conversion task|no conversion tasks/);
  });
}
