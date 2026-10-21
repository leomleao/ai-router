// Real Vercel SDK versions from n8n's catalog; no installed n8n app or real AGY.
// Run beside the Rust fake gateway in the same network-none Docker container.
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
const require = createRequire('/opt/n8n-contract/package.json');
const { generateText, streamText, tool, jsonSchema } = await import(require.resolve('ai'));
const { createOpenAI } = await import(require.resolve('@ai-sdk/openai'));

let configuration = '';
for await (const chunk of process.stdin) configuration += chunk;
const {base_url: BASE, model: MODEL} = JSON.parse(configuration);
assert.ok(BASE.startsWith('http://127.0.0.1:'));
const KEY = 'test-key';
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
    assert.equal(url.origin, new URL(BASE).origin, 'SDK may contact only the synthetic loopback gateway');
    const body = typeof init?.body === 'string'
      ? JSON.parse(init.body)
      : input instanceof Request ? await input.clone().json() : undefined;
    const response = await fetch(input, init);
    wire.push({ path: url.pathname, body, status: response.status,
      mode: response.headers.get('x-ai-router-token-budget-mode'),
      cacheControls: response.headers.get('x-ai-router-prompt-cache-controls'),
      itemStorage: response.headers.get('x-ai-router-item-storage') });
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
    ...(neutralDefaults ? { providerOptions: { openai: {
      textVerbosity: 'medium', store: false,
      promptCacheKey: 'synthetic-n8n-cache-key', promptCacheRetention: '24h',
    } } } : {}),
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
    check(request.body.prompt_cache_key === 'synthetic-n8n-cache-key'
      && request.body.prompt_cache_retention === '24h', `${style}: SDK sends n8n cache hints on the wire`);
    check(request.cacheControls === 'ignored', `${style}: cache hints are explicitly disclosed as ignored`);
  } else {
    check(verbosity === undefined && request.body.store === undefined,
      `${style}: exact setup probe adds no artificial verbosity/store provider options`);
    check(request.cacheControls === null, `${style}: no ignored-cache header without hints`);
  }
}

function checkResult(result, style) {
  check(result.text === 'Hello from fixture', `${style}: full deterministic fixture text is preserved`);
  check(result.finishReason === 'stop', `${style}: budget does not fabricate a length stop`);
  check(result.usage.inputTokens === 4 && result.usage.outputTokens === 6 && result.usage.totalTokens === 10,
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
  const cached = await generateText(options(style, 16, true));
  checkWire(style, 16, false, true);
  checkResult(cached, `${style} with cache hints`);
  wire = [];
  const result = streamText(options(style, 16, true));
  let text = '';
  for await (const chunk of result.textStream) text += chunk;
  checkWire(style, 16, true, true);
  checkResult({ text, finishReason: await result.finishReason, usage: await result.usage }, `${style} stream`);
}

// Preserve the SDK's provider item IDs, exactly as n8n does between turns.
// Omitted store uses the SDK's default reference behavior, not a forced replay.
async function turn(messages, streaming, store) {
  const opts = { model: provider.responses(MODEL), messages,
    maxRetries: 0, abortSignal: AbortSignal.timeout(30_000),
    ...(store === undefined ? {} : { providerOptions: { openai: { store } } }) };
  if (!streaming) return generateText(opts);
  const result = streamText(opts);
  let text = '';
  for await (const chunk of result.textStream) text += chunk;
  return { text, response: await result.response, usage: await result.usage };
}
for (const streaming of [false, true]) {
  for (const store of [undefined, true, false]) {
    wire = [];
    const initial = [{ role: 'user', content: 'Hello' }];
    const first = await turn(initial, streaming, store);
    check(first.text === 'Hello from fixture', 'First default-SDK turn succeeds');
    const next = await turn([...initial, ...first.response.messages,
      { role: 'user', content: '__fake_referenced_history__' }], !streaming, store);
    check(wire.length === 2 && wire.every(r => r.status === 200), 'Both SDK turns succeed without retries');
    check(wire[1].body.input.some(i => i.type === 'item_reference') === (store !== false),
      'SDK default/true sends actual references; explicit false replays full messages');
    check(wire.every(r => r.itemStorage === (store === false ? 'disabled' : 'memory')),
      'Both response modes disclose memory-only item storage');
    check(next.text === 'Hello from fixture', 'Referenced assistant text reaches the fake provider intact');
    check(next.usage.totalTokens === 10, 'Reference expansion does not fabricate usage');
  }
}


// Workflow agents use a compact tool inventory; assistants commonly send a
// larger inventory with discovery/workspace/CRM tools. Both must retain the
// exact public function names, arguments, IDs and result continuation.
const lookupSchema = {$schema:'http://json-schema.org/draft-07/schema#',type:'object',
  properties:{q:{$ref:'#/$defs/Query'}},$defs:{Query:{type:'string',minLength:1}},
  required:['q'],additionalProperties:false};
for (const inventory of ['workflow-agent','assistant']) {
  const tools = {lookup:tool({description:'Search records',inputSchema:jsonSchema(lookupSchema)})};
  const names = inventory === 'workflow-agent' ? ['HTTP_Request','Wikipedia','Code_Tool'] :
    ['load_skill','web_search','workspace_read','workspace_write','hubspot_search','twenty_snapshot_codec',
     'create_subagent','list_todos','update_todo','get_environment','chat_history','Outlook'];
  for (const name of names) tools[name]=tool({description:name,inputSchema:jsonSchema({type:'object',
    properties:{input:{type:'string'}},required:['input'],additionalProperties:false})});
  // Real n8n discovery tools can accept node names or structured node selectors.
  // The provider mock must reject mixed-type CCA fields, not just unknown keys.
  tools.nodes=tool({description:'Find node types',inputSchema:jsonSchema({type:'object',
    properties:{nodeTypes:{type:'array',items:{anyOf:[{type:'string'},
      {type:'object',properties:{name:{type:'string'}},required:['name']}]}}},
    required:['nodeTypes'],additionalProperties:false})});
  for (const style of ['chat','responses']) for (const streaming of [false,true]) {
    wire=[];
    const messages=[{role:'user',content:'tools-fixture'}];
    const opts={model:style==='chat'?provider.chat(MODEL):provider.responses(MODEL),messages,tools,maxRetries:0,
      abortSignal:AbortSignal.timeout(30_000)};
    let first;
    if (streaming) {
      const result=streamText(opts);
      for await (const _ of result.fullStream) {};
      first={toolCalls:await result.toolCalls,response:await result.response,finishReason:await result.finishReason};
    } else first=await generateText(opts);
    check(first.toolCalls.length===2,`${inventory}/${style}/${streaming}: parallel client calls`);
    check(first.toolCalls.every(c=>c.toolName==='lookup'), 'Exact declared tool names');
    check(JSON.stringify(first.toolCalls.map(c=>c.input))===JSON.stringify([{q:'one'},{q:'two'}]), 'Original argument objects');
    check(first.finishReason==='tool-calls','Tool handoff finish reason');
    const follow=[...messages,...first.response.messages,{role:'tool',content:first.toolCalls.map(c=>({
      type:'tool-result',toolCallId:c.toolCallId,toolName:c.toolName,output:{type:'text',value:'found'},
    }))}];
    // Assistant discovery can change the next turn's tool inventory.
    const {lookup: _previous, ...remaining}=tools;
    const continuationTools=inventory==='assistant'?{...remaining,get_person:tool({description:'Newly discovered CRM tool',
      inputSchema:jsonSchema({type:'object',properties:{id:{type:'string'}},required:['id']})})}:tools;
    let result;
    if (!streaming) {
      const streamed=streamText({...opts,messages:follow,tools:continuationTools});
      let text=''; for await (const chunk of streamed.textStream) text+=chunk;
      result={text};
    } else result=await generateText({...opts,messages:follow,tools:continuationTools});
    check(result.text==='tool results accepted',`${inventory}/${style}: client-executed results round trip`);
    check(wire.length===2 && wire.every(r=>r.status===200),'No hidden retries');
  }
}
console.log(`PASS: ${checks} n8n Vercel SDK checks using ai7.0.66/openai4.0.20 and synthetic AGY`);
