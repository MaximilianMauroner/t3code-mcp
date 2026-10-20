import assert from "node:assert/strict";

const inputSchema = {
  type: "object",
  properties: {
    cursor: { type: "integer", minimum: 0 },
    limit: { type: "integer", minimum: 1, maximum: 100 },
    projectId: { type: "string" },
    filters: { type: "object", properties: {
      statuses: { type: "array", items: { type: "string", enum: ["running", "completed", "waiting"] } },
      archived: { type: "boolean", default: false },
    } },
  },
};
const outputSchema = {
  type: "object",
  properties: { marker: { type: "string" }, items: { type: "array", items: { type: "string" } } },
  required: ["marker", "items"],
};

function checkSchema(actual, expected, runtime, location) {
  assert(actual && typeof actual === "object" && !Array.isArray(actual), location);
  const comparable = { ...actual };
  if (runtime === "typescript") {
    for (const key of ["$schema", "title", "description"]) delete comparable[key];
    if (expected.type === "object" && (comparable.additionalProperties === true
      || (comparable.additionalProperties && typeof comparable.additionalProperties === "object"
        && !Array.isArray(comparable.additionalProperties) && Object.keys(comparable.additionalProperties).length === 0))) {
      delete comparable.additionalProperties;
    }
    if (expected.type === "integer") {
      if (expected.minimum === undefined && comparable.minimum === Number.MIN_SAFE_INTEGER) delete comparable.minimum;
      if (expected.maximum === undefined && comparable.maximum === Number.MAX_SAFE_INTEGER) delete comparable.maximum;
    }
  }
  if (expected.properties) {
    assert(comparable.properties && typeof comparable.properties === "object", location);
    assert.deepEqual(Object.keys(comparable.properties).sort(), Object.keys(expected.properties).sort(), location);
    for (const [key, schema] of Object.entries(expected.properties)) {
      checkSchema(comparable.properties[key], schema, runtime, `${location}.properties.${key}`);
    }
    comparable.properties = expected.properties;
  }
  if (expected.items) {
    checkSchema(comparable.items, expected.items, runtime, `${location}.items`);
    comparable.items = expected.items;
  }
  assert.deepEqual(comparable, expected, location);
}

export function createCatalogFixture({ NATIVE_TOOL_NAMES, TOOL_NAMES }) {
  assert.equal(new Set(NATIVE_TOOL_NAMES).size, NATIVE_TOOL_NAMES.length, "Duplicate native fixture names.");
  assert.equal(new Set(TOOL_NAMES).size, TOOL_NAMES.length, "Duplicate expected tool names.");
  const tools = NATIVE_TOOL_NAMES.map((name) => ({
    name, description: `Isolated memory fixture for ${name}.`, inputSchema, outputSchema,
  }));
  return {
    tools,
    checkCatalog(catalog, runtime) {
      assert(["typescript", "rust"].includes(runtime), "Unknown gateway runtime.");
      assert(Array.isArray(catalog.tools), "Missing gateway catalog.");
      assert.deepEqual(catalog.tools.map((tool) => tool.name).sort(), [...TOOL_NAMES].sort(), "Complete gateway tool names.");
      const byName = new Map(catalog.tools.map((tool) => [tool.name, tool]));
      for (const expected of tools) {
        const actual = byName.get(expected.name);
        assert.equal(actual.description, expected.description, `${expected.name}.description`);
        checkSchema(actual.inputSchema, inputSchema, runtime, `${expected.name}.inputSchema`);
        if (runtime === "rust" || actual.outputSchema !== undefined) {
          checkSchema(actual.outputSchema, outputSchema, runtime, `${expected.name}.outputSchema`);
        }
      }
    },
  };
}
