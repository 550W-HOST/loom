import { describe, expect, it } from "vitest";
import { availableModelSchema } from "./provider-types.js";

/**
 * The catalogue the server serves is the agent's own, and the persisted
 * "last known models" cache parses it with this schema. A level id bb's
 * vocabulary never named must not make that parse fail: a rejected catalogue
 * would silently disable the cache for every model the agent describes.
 */
describe("availableModelSchema", () => {
  const model = {
    id: "pi/mock-plain",
    model: "mock/plain",
    displayName: "Plain",
    description: "Plain",
    supportedReasoningEfforts: [
      { reasoningEffort: "off", description: "No reasoning" },
      { reasoningEffort: "minimal", description: "A little reasoning" },
    ],
    defaultReasoningEffort: "off",
    isDefault: true,
  };

  it("accepts an agent's own level ids, including ones bb never named", () => {
    expect(availableModelSchema.safeParse(model).success).toBe(true);
  });

  it("still refuses an empty level id", () => {
    const parsed = availableModelSchema.safeParse({
      ...model,
      defaultReasoningEffort: "",
    });
    expect(parsed.success).toBe(false);
  });
});
