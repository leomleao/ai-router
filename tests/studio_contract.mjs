// Supabase Studio assistant shape: ai 7.0.59 / @ai-sdk/openai 4.0.41, pinned
// from supabase/studio:2026.10.12. Synthetic loopback gateway only.
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
const require = createRequire('/opt/studio-contract/package.json');
const { generateText, streamText, tool, jsonSchema, isStepCount } = await import(require.resolve('ai'));
const { createOpenAI } = await import(require.resolve('@ai-sdk/openai'));

let configuration = '';
for await (const chunk of process.stdin) configuration += chunk;
const {base_url: BASE, model: MODEL} = JSON.parse(configuration);
assert.ok(BASE.startsWith('http://127.0.0.1:'));
// Studio's default assistant model. The SDK derives reasoning behaviour from
// this ID; the fetch below only retargets it to the synthetic catalogue.
const STUDIO_MODEL = 'gpt-6-luna';
let checks = 0;
let wire = [];
function check(condition, message) {
  assert.ok(condition, message);
  checks += 1;
}

check(require('ai/package.json').version === '7.0.59', 'Pinned Studio AI SDK version');
check(require('@ai-sdk/openai/package.json').version === '4.0.41', 'Pinned Studio OpenAI provider version');

const provider = createOpenAI({
  apiKey: 'test-key',
  baseURL: BASE,
  fetch: async (input, init) => {
    const url = new URL(input instanceof Request ? input.url : String(input));
    assert.equal(url.origin, new URL(BASE).origin, 'SDK may contact only the synthetic loopback gateway');
    const body = JSON.parse(init.body);
    const response = await fetch(input, { ...init, body: JSON.stringify({ ...body, model: MODEL }) });
    wire.push({ path: url.pathname, body, status: response.status,
      summary: response.headers.get('x-ai-router-reasoning-summary'),
      itemStorage: response.headers.get('x-ai-router-item-storage'),
      sampling: response.headers.get('x-ai-router-sampling-controls'),
      effort: response.headers.get('x-ai-router-reasoning-effort') });
    return response;
  },
});

// Studio getModel(): registry store=false plus the entry's reasoning effort.
const providerOptions = { openai: { store: false, reasoningEffort: 'medium' } };
const tools = {
  lookup: tool({
    description: 'Search records',
    inputSchema: jsonSchema({ type: 'object', properties: { q: { type: 'string' } },
      required: ['q'], additionalProperties: false }),
    execute: async () => ({ rows: [] }),
  }),
  load_knowledge: tool({
    description: 'Load knowledge',
    inputSchema: jsonSchema({ type: 'object', properties: { name: { type: 'string' } },
      required: ['name'], additionalProperties: false }),
    execute: async () => 'knowledge',
  }),
};
const reasoningProvider = !MODEL.startsWith('gemini-') && !MODEL.startsWith('claude-');

function checkFirstRequest(request, streaming) {
  check(request.path === '/v1/responses' && request.status === 200, 'Studio uses the Responses endpoint');
  check(request.body.model === STUDIO_MODEL && Boolean(request.body.stream) === streaming,'Exact SDK model and stream shape');
  check(request.body.store === false, 'Registry store=false reaches the wire');
  check(JSON.stringify(request.body.include) === '["reasoning.encrypted_content"]',
    'SDK adds encrypted reasoning include for store=false reasoning models');
  check(request.body.reasoning?.effort === 'medium' && request.body.reasoning?.summary === 'detailed',
    'SDK adds a detailed summary request to the configured effort');
  check(request.body.tool_choice === 'auto' && request.body.tools.length === 2, 'Default tool choice and inventory');
  check(request.body.input[0].role === 'developer', 'Studio instructions become a developer message');
  check(request.body.temperature === undefined && request.body.max_output_tokens === undefined,
    'No sampling or token controls are sent');
  check(request.summary === 'unavailable' && request.itemStorage === 'disabled',
    'Unfulfilled summaries and disabled storage are disclosed');
}

function checkContinuation(request) {
  const input = request.body.input;
  check(request.status === 200, 'Tool continuation succeeds');
  check(input.filter(item => item.type === 'function_call_output').length === 2, 'Both executed results replay');
  check(!input.some(item => item.type === 'item_reference'), 'store=false replays full items, not references');
  const reasoning = input.filter(item => item.type === 'reasoning');
  check(reasoningProvider
    ? reasoning.length === 1 && reasoning[0].encrypted_content === 'synthetic-encrypted-reasoning'
      && reasoning[0].summary.length === 0
    : reasoning.length === 0, 'Only provider-returned encrypted reasoning is replayed');
}

for (const streaming of [true, false]) {
  wire = [];
  const options = { model: provider(STUDIO_MODEL),
    instructions: { role: 'system', content: 'Synthetic Studio system prompt' },
    stopWhen: isStepCount(20),
    messages: [{ role: 'user', content: 'tools-fixture' }],
    providerOptions, tools, maxRetries: 0, abortSignal: AbortSignal.timeout(30_000) };
  let result;
  if (streaming) {
    const stream = streamText(options);
    const parts = [];
    for await (const part of stream.fullStream) parts.push(part);
    check(!parts.some(part => part.type === 'error'), 'Studio stream has no error part');
    result = { text: await stream.text, steps: await stream.steps, finishReason: await stream.finishReason };
  } else result = await generateText(options);
  check(wire.length === 2, 'One tool step and one answer step, without retries');
  checkFirstRequest(wire[0], streaming);
  checkContinuation(wire[1]);
  check(result.text === 'tool results accepted' && result.finishReason === 'stop', 'Final answer after tool step');
  check(result.steps[0].toolCalls.length === 2
    && result.steps[0].toolCalls.every(call => call.toolName === 'lookup'), 'Exact tool calls');
  const reasoning = result.steps[0].reasoning ?? [];
  check(reasoning.every(part => part.text === ''), 'No reasoning summary text is fabricated');
  check(reasoningProvider ? reasoning.length === 1 : reasoning.length === 0, 'Reasoning parts match the provider');
}

// Studio's simple endpoints (titles, SQL edits, feedback classification) use
// effort "none"; feedback classification also sets temperature 0.
wire = [];
const simple = await generateText({ model: provider(STUDIO_MODEL), prompt: 'hello', temperature: 0,
  maxRetries: 0, providerOptions: { openai: { store: false, reasoningEffort: 'none' } } });
check(wire.length === 1 && wire[0].status === 200 && simple.text.length > 0, 'Effort none request succeeds');
check(wire[0].body.reasoning?.effort === 'none' && wire[0].body.reasoning?.summary === undefined
  && JSON.stringify(wire[0].body.include) === '["reasoning.encrypted_content"]' && wire[0].body.temperature === 0,
  'SDK sends effort none, encrypted include and temperature');
check(wire[0].sampling === 'ignored', 'Ignored temperature is disclosed');
// The fixture catalogue lists low/medium/high, so OpenAI gets its lowest level.
check(wire[0].effort === (reasoningProvider ? 'mapped=low' : 'ignored'),
  'OpenAI maps unlisted effort none to the lowest listed level; Antigravity discloses that the model ID decides');

// Other include values remain explicit errors, never silently dropped.
wire = [];
await assert.rejects(generateText({ model: provider(STUDIO_MODEL), prompt: 'hello', maxRetries: 0,
  providerOptions: { openai: { ...providerOptions.openai, include: ['file_search_call.results'] } } }),
  error => error.statusCode === 400 && error.responseBody.includes('unsupported_parameter')
    && error.responseBody.includes('include'));
check(wire.length === 1 && wire[0].body.include.includes('file_search_call.results'), 'Unsupported include is rejected');
console.log(`PASS: ${checks} Supabase Studio SDK checks using ai7.0.59/openai4.0.41`);
