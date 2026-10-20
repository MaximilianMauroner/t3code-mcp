import assert from "node:assert/strict";
import test from "node:test";
import { createCatalogFixture } from "./gateway-memory-catalog.mjs";

const contract = { NATIVE_TOOL_NAMES: ["t3_thread_list", "t3_thread_read"], TOOL_NAMES: ["t3_threads_overview", "t3_thread_list", "t3_thread_read"] };

function fixture() {
  const { tools, checkCatalog } = createCatalogFixture(contract);
  const catalog = { tools: JSON.parse(JSON.stringify([...tools, { name: "t3_threads_overview" }])) };
  return { catalog, checkCatalog };
}

test("accepts the complete Rust catalog and known TypeScript schema representations", () => {
  const { catalog, checkCatalog } = fixture();
  checkCatalog(catalog, "rust");
  for (const tool of catalog.tools.slice(0, 2)) {
    delete tool.outputSchema;
    tool.inputSchema.$schema = "https://json-schema.org/draft/2020-12/schema";
    tool.inputSchema.additionalProperties = {};
    tool.inputSchema.properties.filters.additionalProperties = true;
    tool.inputSchema.properties.cursor.maximum = Number.MAX_SAFE_INTEGER;
    tool.inputSchema.properties.filters.description = "Converted schema metadata";
  }
  checkCatalog(catalog, "typescript");
});

const corruptions = {
  "missing native tool": (catalog) => { catalog.tools.splice(1, 1); },
  "duplicate native tool": (catalog) => { catalog.tools.push(catalog.tools[1]); },
  "missing gateway tool": (catalog) => { catalog.tools.pop(); },
  "erased uncalled schema": (catalog) => { catalog.tools[1].inputSchema = { type: "object" }; },
  "missing nested property": (catalog) => { delete catalog.tools[1].inputSchema.properties.filters.properties.archived; },
  "wrong type": (catalog) => { catalog.tools[1].inputSchema.properties.cursor.type = "number"; },
  "missing array items": (catalog) => { delete catalog.tools[1].inputSchema.properties.filters.properties.statuses.items; },
  "missing enum value": (catalog) => { catalog.tools[1].inputSchema.properties.filters.properties.statuses.items.enum.pop(); },
  "missing explicit minimum": (catalog) => { delete catalog.tools[1].inputSchema.properties.cursor.minimum; },
  "changed explicit maximum": (catalog) => { catalog.tools[1].inputSchema.properties.limit.maximum = Number.MAX_SAFE_INTEGER; },
  "missing default": (catalog) => { delete catalog.tools[1].inputSchema.properties.filters.properties.archived.default; },
  "restricted unknown fields": (catalog) => { catalog.tools[1].inputSchema.additionalProperties = false; },
};
for (const [name, corrupt] of Object.entries(corruptions)) {
  test(`rejects ${name} in either gateway catalog`, () => {
    for (const runtime of ["typescript", "rust"]) {
      const { catalog, checkCatalog } = fixture();
      corrupt(catalog);
      assert.throws(() => checkCatalog(catalog, runtime));
    }
  });
}

test("requires the Rust native output schema", () => {
  const { catalog, checkCatalog } = fixture();
  delete catalog.tools[1].outputSchema.properties.items.items;
  assert.throws(() => checkCatalog(catalog, "rust"));
});

test("rejects duplicate native fixture names before measurement", () => {
  assert.throws(() => createCatalogFixture({ ...contract, NATIVE_TOOL_NAMES: ["t3_thread_list", "t3_thread_list"] }));
});
