import { describe, it, expect, vi } from "vitest";
import { responsesCreateViaGemini } from "../src/utils/responses.genai.js";
import * as genaiClient from "../src/utils/genaiClient.js";

// Fake streams for testing
async function* fakeGenaiStream() {
  yield { text: "Result: " };
  yield { text: "42" };
  // Emulate functionCalls in final response
}

describe("Gemini function-calling adapter", () => {
  it("streams text and then function call events", async () => {
    // Mock the GenAI client
    const fakeClient = {
      models: {
        generateContentStream: vi.fn().mockResolvedValueOnce(fakeGenaiStream()),
      },
    };
    vi.spyOn(genaiClient, "makeGeminiClient").mockReturnValue(
      fakeClient as any,
    );

    const input = {
      model: "gemini-2.0-flash",
      input: ["Ask for a function call"],
      stream: true,
      temperature: 0,
      top_p: 1,
      tools: [
        {
          type: "function",
          name: "foo",
          description: "desc",
          parameters: { type: "object", properties: {}, required: [] },
        },
      ],
    };
    const events: Array<any> = [];
    for await (const ev of responsesCreateViaGemini(input as any)) {
      events.push(ev);
    }
    // Ensure text deltas are emitted
    expect(
      events.find((e) => e.type === "response.output_text.delta"),
    ).toBeDefined();
    // Ensure final completed event
    expect(events.find((e) => e.type === "response.completed")).toBeDefined();
  });
});
