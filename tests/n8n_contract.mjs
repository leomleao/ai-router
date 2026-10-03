// Real Vercel SDK versions from n8n's catalog; no installed n8n app or real AGY.
// Run beside the Rust fake gateway in the same network-none Docker container.
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { generateText, streamText } from 'ai';
import { createOpenAI } from '@ai-sdk/openai';

const require = createRequire(import.meta.url);
const BASE = 'http://127.0.0.1:8080/v1';
const MODEL = 'gemini-3-pro';
const KEY = 'synthetic-model-test-key';
let checks = 0;
let wire = [];
function check(condition, message) {
  assert.ok(condition, message);
  checks += 1;
}

check(require('ai/package.json').version === '7.0.66', 'Pinned n8n AI SDK version');
check(require('@ai-sdk/openai/package.json').version === '4.0.20', 'Pinned OpenAI provider version');

const provider = createOpenAI({
  apiKey: KEY,
  baseURL: BASE,
  fetch: async (input, init) => {
    const url = new URL(input instanceof Request ? input.url : String(input));
    assert.equal(url.origin, 'http://127.0.0.1:8080', 'SDK may contact only the synthetic loopback gateway');
    const body = typeof init?.body === 'string'
      ? JSON.parse(init.body)
      : input instanceof Request ? await input.clone().json() : undefined;
    const response = await fetch(input, init);
    wire.push({ path: url.pathname, body, status: response.status,
      mode: response.headers.get('x-ai-router-token-budget-mode') });
    return response;
  },
});

function options(style, budget, neutralDefaults = false) {
  return {
    model: style === 'chat' ? provider.chat(MODEL) : provider.responses(MODEL),
    // The unaugmented call matches n8n credential verification. Streaming
    // separately exercises the gateway's accepted neutral provider defaults.
    prompt: 'Reply with OK.',
    maxOutputTokens: budget,
    abortSignal: AbortSignal.timeout(30_000),
    maxRetries: 0,
    ...(neutralDefaults ? { providerOptions: { openai: { textVerbosity: 'medium', store: false } } } : {}),
  };
}

function checkWire(style, budget, streaming, neutralDefaults = false) {
  check(wire.length === 1, `${style}: one SDK request, without retry or external calls`);
  const request = wire[0];
  check(request.path === `/v1/${style === 'chat' ? 'chat/completions' : 'responses'}`,
    `${style}: expected endpoint`);
  check(request.body[style === 'chat' ? 'max_tokens' : 'max_output_tokens'] === budget,
    `${style}: SDK maps maxOutputTokens=${budget} to the actual wire field`);
  check(Boolean(request.body.stream) === streaming, `${style}: requested streaming shape`);
  check(request.status === 200 && request.mode === 'prompt-guidance', `${style}: honest best-effort header`);
  const verbosity = style === 'responses' ? request.body.text?.verbosity : request.body.verbosity;
  if (neutralDefaults) {
    check(verbosity === 'medium' && request.body.store === false, `${style}: separately requested neutral defaults travel with the budget`);
  } else {
    check(verbosity === undefined && request.body.store === undefined,
      `${style}: exact setup probe adds no artificial verbosity/store provider options`);
  }
}

function checkResult(result, style) {
  check(result.text === 'Hello from fixture', `${style}: full deterministic fixture text is preserved`);
  check(result.finishReason === 'stop', `${style}: budget does not fabricate a length stop`);
  check(result.usage.inputTokens === 20 && result.usage.outputTokens === 3 && result.usage.totalTokens === 23,
    `${style}: observed provider usage is unchanged`);
}

for (const style of ['chat', 'responses']) {
  for (const budget of [8, 16]) {
    wire = [];
    const result = await generateText(options(style, budget));
    checkWire(style, budget, false);
    checkResult(result, style);
  }
  wire = [];
  const result = streamText(options(style, 16, true));
  let text = '';
  for await (const chunk of result.textStream) text += chunk;
  checkWire(style, 16, true, true);
  checkResult({ text, finishReason: await result.finishReason, usage: await result.usage }, `${style} stream`);
}

console.log(`PASS: ${checks} n8n Vercel SDK checks using ai7.0.66/openai4.0.20 and synthetic AGY`);
