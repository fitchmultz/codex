import { getApiKey } from "./config";
import { GoogleGenAI } from "@google/genai";

/**
 * Create and configure a Gemini (Google GenAI) client.
 *
 * @throws {Error} if the GEMINI_API_KEY environment variable is not set.
 */
export function makeGeminiClient(): GoogleGenAI {
  const apiKey = getApiKey("gemini");
  if (!apiKey) {
    throw new Error(
      "Missing GEMINI_API_KEY; please set it in your environment.",
    );
  }
  return new GoogleGenAI({ apiKey });
}
