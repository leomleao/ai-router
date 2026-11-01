// Pinned n8n Vercel and LangChain clients against a synthetic loopback gateway.
// The only input is {base_url, model}; no live provider or external endpoint is used.
import assert from 'node:assert/strict';
import {createRequire} from 'node:module';

const n8nRequire = createRequire('/opt/n8n-contract/package.json');
const studioRequire = createRequire('/opt/studio-contract/package.json');
const n8nAI = await import(n8nRequire.resolve('ai'));
const n8nOpenAI = await import(n8nRequire.resolve('@ai-sdk/openai'));
const studioAI = await import(studioRequire.resolve('ai'));
const studioOpenAI = await import(studioRequire.resolve('@ai-sdk/openai'));
const {ChatOpenAI} = await import(n8nRequire.resolve('@langchain/openai'));
const {HumanMessage} = await import(n8nRequire.resolve('@langchain/core/messages'));

assert.equal(n8nRequire('ai/package.json').version, '7.0.66');
assert.equal(n8nRequire('@ai-sdk/openai/package.json').version, '4.0.20');
assert.equal(n8nRequire('@langchain/openai/package.json').version, '1.4.4');
assert.equal(studioRequire('ai/package.json').version, '7.0.59');
assert.equal(studioRequire('@ai-sdk/openai/package.json').version, '4.0.41');

let input = '';
for await (const chunk of process.stdin) input += chunk;
const {base_url: BASE, model: MODEL} = JSON.parse(input);
const base = new URL(BASE);
assert.equal(base.protocol, 'http:');
assert.equal(base.hostname, '127.0.0.1');
assert.ok(base.port);

let checks = 0;
let wire = [];
function check(value, message) {
  assert.ok(value, message);
  checks += 1;
}

const PNG = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/T9sAAAAASUVORK5CYII=';
const PNG_DATA = `data:image/png;base64,${PNG}`;
function oneRequest(path, verify) {
  check(wire.length === 1, 'one client request, with retries disabled');
  const request = wire.pop();
  check(request.path === path && request.status === 200, `expected successful ${path} request`);
  check(verify(request.body), `SDK request maps inputs correctly: ${JSON.stringify(request.body)}`);
}

function hasInputImage(body) {
  return body.input?.some(item => (item.type ?? 'message') === 'message' && item.content?.some(part =>
    part.type === 'input_image' && part.image_url === PNG_DATA));
}

const vercelClients = [
  {name: 'n8n', ai: n8nAI, openai: n8nOpenAI},
  {name: 'Studio', ai: studioAI, openai: studioOpenAI},
];

for (const sdk of vercelClients) {
  const {generateText, streamText} = sdk.ai;
  const {createOpenAI} = sdk.openai;
  const provider = createOpenAI({
    apiKey: 'test-key',
    baseURL: BASE,
    fetch: async (input, init) => {
      const url = new URL(input instanceof Request ? input.url : String(input));
      assert.equal(url.origin, base.origin, 'Vercel SDK may contact only the synthetic loopback gateway');
      const body = typeof init?.body === 'string'
        ? JSON.parse(init.body)
        : input instanceof Request ? await input.clone().json() : undefined;
      const response = await fetch(input, init);
      wire.push({path: url.pathname, body, status: response.status});
      return response;
    },
  });

for (const streaming of [false, true]) {
  wire = [];
  const messages = [{role: 'user', content: [
    {type: 'text', text: 'Describe this synthetic image.'},
    streaming ? {type: 'file', data: Buffer.from(PNG, 'base64'), mediaType: 'image/png'}
      : {type: 'image', image: Buffer.from(PNG, 'base64'), mediaType: 'image/png'},
  ]}];
  if (!streaming) {
    const result = await generateText({model: provider.responses(MODEL), messages,
      maxRetries: 0, abortSignal: AbortSignal.timeout(20_000)});
    check(result.text === 'OpenAI synthetic answer', 'Vercel Responses vision result');
  } else {
    const result = streamText({model: provider.responses(MODEL), messages,
      maxRetries: 0, abortSignal: AbortSignal.timeout(20_000)});
    let text = '';
    for await (const part of result.textStream) text += part;
    check(text === 'OpenAI synthetic answer', 'Vercel Responses vision stream');
    check(await result.finishReason === 'stop', 'Vercel vision stream completes normally');
  }
  oneRequest('/v1/responses', hasInputImage);
}

// The OpenAI provider represents native Responses web search as a provider
// tool. Its adapter automatically requests web_search_call.action.sources.
const hostedTools = {web_search: provider.tools.webSearch()};
for (const streaming of [false, true]) {
  wire = [];
  let result;
  let streamSources = [];
  if (!streaming) {
    result = await generateText({model: provider.responses(MODEL),
      prompt: 'web_search contract fixture', tools: hostedTools,
      maxRetries: 0, abortSignal: AbortSignal.timeout(20_000)});
  } else {
    result = streamText({model: provider.responses(MODEL),
      prompt: 'web_search contract fixture', tools: hostedTools,
      maxRetries: 0, abortSignal: AbortSignal.timeout(20_000)});
    for await (const part of result.fullStream) {
      if (part.type === 'source') streamSources.push(part);
      if (part.type === 'error') throw part.error;
    }
  }
  check((streaming ? await result.text : result.text) === 'Search fixture answer',
    `Vercel ${streaming ? 'streamed' : 'nonstreamed'} hosted-search answer`);
  const sources = streaming ? await result.sources : result.sources;
  check(sources.some(source => source.sourceType === 'url'
    && source.url === 'https://example.test/source' && source.title === 'Fixture source'),
  'Vercel Responses exposes the exact citation through result.sources');
  if (streaming) {
    check(streamSources.some(source => source.sourceType === 'url'
      && source.url === 'https://example.test/source' && source.title === 'Fixture source'),
    'Vercel stream emits the URL source event');
  }
  const calls = streaming ? await result.toolCalls : result.toolCalls;
  check(calls.some(call => call.toolName === 'web_search' && call.providerExecuted === true),
    'hosted web search remains a provider-executed tool, not a custom handoff');
  oneRequest('/v1/responses', body => body.tools?.some(tool => tool.type === 'web_search')
    && body.include?.includes('web_search_call.action.sources'));
}
}

// n8n LangChain ChatOpenAI vision path. Verify both the SDK's wire form and
// its public result; maxRetries=0 keeps each operation to one synthetic call.
const langWire = [];
const fetchLocal = async (url, init) => {
  const parsed = new URL(url);
  assert.equal(parsed.origin, base.origin, 'LangChain may contact only the synthetic loopback gateway');
  const body = JSON.parse(init.body);
  const response = await fetch(url, init);
  langWire.push({path: parsed.pathname, body, status: response.status});
  return response;
};
const lang = new ChatOpenAI({model: MODEL, apiKey: 'test-key', maxRetries: 0,
  timeout: 10_000, configuration: {baseURL: BASE, fetch: fetchLocal}});
for (const streaming of [false, true]) {
  langWire.length = 0;
  const message = new HumanMessage([
    {type: 'text', text: 'Describe this synthetic image.'},
    {type: 'image_url', image_url: {url: PNG_DATA}},
  ]);
  let text = '';
  if (!streaming) {
    const response = await lang.invoke([message]);
    text = typeof response.content === 'string' ? response.content
      : response.content.filter(part => part.type === 'text').map(part => part.text).join('');
  } else {
    for await (const chunk of await lang.stream([message])) {
      text += typeof chunk.content === 'string' ? chunk.content
        : chunk.content.filter(part => part.type === 'text').map(part => part.text).join('');
    }
  }
  check(text === 'OpenAI synthetic answer', `LangChain ${streaming ? 'streamed' : 'nonstreamed'} Chat vision result`);
  check(langWire.length === 1, 'LangChain vision sends one request without retry');
  const request = langWire.pop();
  check(request.path === '/v1/chat/completions' && request.status === 200,
    'LangChain uses Chat Completions successfully');
  check(request.body.messages?.some(item => item.role === 'user' && item.content?.some(part =>
    part.type === 'image_url' && part.image_url?.url === PNG_DATA)),
  'LangChain image_url message maps to the Chat wire request');
}

console.log(`PASS: ${checks} n8n Vercel/LangChain media and hosted-search checks`);
