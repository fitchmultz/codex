import { describe, it, expect, vi } from "vitest";
import { nonStreamResponses } from "../src/utils/responses.js";
import { responsesCreateViaGemini } from "../src/utils/responses.genai.js";
import * as genaiClient from "../src/utils/genaiClient.js";

// A minimal fake ChatCompletion type for nonStreamResponses
const makeChatCompletion = (opts: any) => ({
  model: opts.model ?? "gemini-2.0",
  choices: [{ message: opts.message }],
  usage: opts.usage,
});

describe("Gemini streaming adapter", () => {
  it("yields correct text delta and final events", async () => {
    // Arrange: stub makeGeminiClient
    const fakeChunks = [{ text: "Hello " }, { text: "World" }];
    async function* fakeStream() {
      for (const c of fakeChunks) {
        yield c;
      }
    }
    const fakeClient = {
      models: {
        generateContentStream: vi.fn().mockResolvedValue(fakeStream()),
      },
    };
    vi.spyOn(genaiClient, "makeGeminiClient").mockReturnValue(
      fakeClient as any,
    );

    // Act: collect events
    const input = {
      model: "gemini-2.0",
      tools: [],
      stream: true,
      temperature: 0,
      top_p: 1,
    } as any;
    const events: Array<any> = [];
    for await (const e of responsesCreateViaGemini(input)) {
      events.push(e);
    }

    // Assert: first events are response.created and in_progress
    expect(events[0].type).toBe("response.created");
    expect(events[1].type).toBe("response.in_progress");
    // Then content part added
    expect(events.some((e) => e.type === "response.content_part.added")).toBe(
      true,
    );
    // Then delta for 'Hello '
    expect(
      events.some(
        (e) => e.type === "response.output_text.delta" && e.delta === "Hello ",
      ),
    ).toBe(true);
    // Then delta for 'World'
    expect(events.some((e) => e.delta === "World")).toBe(true);
    // Then final done and completed events
    expect(events.some((e) => e.type === "response.completed")).toBe(true);
  });
});

describe("Gemini non-streaming adapter", () => {
  it("transforms tool_calls into required_action on response", async () => {
    // Prepare a fake assistant message with tool_calls and content
    const toolCalls = [
      {
        id: "call1",
        type: "function",
        function: { name: "foo", arguments: "bar" },
      },
    ];
    const assistantMessage = {
      role: "assistant",
      content: "ignored",
      tool_calls: toolCalls,
    };
    const chatCompletion = makeChatCompletion({
      model: "gemini-2.0",
      message: assistantMessage,
      usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
    });
    const input = {
      model: "gemini-2.0",
      tools: [],
      stream: false,
      temperature: 0,
      top_p: 1,
      previous_response_id: null,
      user: "u",
      metadata: {},
    } as any;
    // Act
    const result = await nonStreamResponses(input, chatCompletion as any);
    // Assert: status requires_action
    expect(result.status).toBe("requires_action");
    // Assert required_action has submit_tool_outputs with our call
    expect(
      (result as any).required_action.submit_tool_outputs.tool_calls,
    ).toEqual([
      {
        id: "call1",
        type: "function",
        function: { name: "foo", arguments: "bar" },
      },
    ]);
  });
});
