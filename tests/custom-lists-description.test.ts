// @ts-expect-error Node test types are intentionally outside the browser-only tsconfig.
import assert from "node:assert/strict";
// @ts-expect-error Node test types are intentionally outside the browser-only tsconfig.
import test from "node:test";
import "./_localstorage-stub.ts";
import { createList, readLists, updateListDetails } from "../src/lib/custom-lists";

test("custom lists persist an optional description", () => {
  const id = createList("Weekend watchlist", "A few films to keep for the weekend.");
  assert.ok(id);

  const list = readLists()[0];
  assert.equal(list?.name, "Weekend watchlist");
  assert.equal(list?.description, "A few films to keep for the weekend.");

  updateListDetails(id!, { description: "Updated notes" });
  const updated = readLists()[0];
  assert.equal(updated?.description, "Updated notes");
});
