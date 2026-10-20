// Pinned Vercel SDK over a real loopback gateway; no external provider calls.
import assert from 'node:assert/strict';
import {createRequire} from 'node:module';
const require = createRequire('/opt/n8n-contract/package.json');
const {generateText, streamText} = await import(require.resolve('ai'));
const {createOpenAI} = await import(require.resolve('@ai-sdk/openai'));
let configuration = '';
for await (const chunk of process.stdin) configuration += chunk;
const {base_url,model: modelId} = JSON.parse(configuration);
assert.ok(base_url.startsWith('http://127.0.0.1:'));
const provider = createOpenAI({apiKey:'test-key',baseURL:base_url});
let checks = 0;
for (const model of [provider.chat(modelId ?? 'gemini-4-argon-medium'),provider.responses(modelId ?? 'gemini-4-argon-medium')]) {
  const answer=await generateText({model,prompt:'hello',maxRetries:0});
  assert.equal(answer.text,'Hello from fixture'); checks++;
  const stream=streamText({model,prompt:'hello',maxRetries:0});
  let text=''; for await (const delta of stream.textStream) text+=delta;
  assert.equal(text,'Hello from fixture'); checks++;
}
console.log(`Pinned Vercel SDK direct TCP checks passed: ${checks}`);
