// n8n workflow AI Agents use LangChain, separately from the assistant's
// Vercel SDK. Pinned to n8n's public pnpm catalog; synthetic loopback only.
import assert from 'node:assert/strict';
import {createRequire} from 'node:module';
const require=createRequire('/opt/n8n-contract/package.json');
const {ChatOpenAI}=await import(require.resolve('@langchain/openai'));
const {HumanMessage,ToolMessage}=await import(require.resolve('@langchain/core/messages'));
let input='';for await (const chunk of process.stdin) input+=chunk;
const {base_url,model,failure}=JSON.parse(input);
assert.ok(base_url.startsWith('http://127.0.0.1:'));
let checks=0;let wire=[];
function check(value,message){assert.ok(value,message);checks++;}
const fetchLocal=async(url,init)=>{
  check(new URL(url).origin===new URL(base_url).origin,'Loopback only');
  const body=JSON.parse(init.body);wire.push(body);
  return fetch(url,init);
};
// LangChain ignores response.failed, so a streamed failure must reach it as
// an HTTP error before acceptance, or as the SDK-raised `error` event after.
if (failure) {
  const llm=new ChatOpenAI({model,apiKey:'test-key',maxRetries:0,timeout:10000,
    useResponsesApi:failure.api==='responses',configuration:{baseURL:base_url,fetch:fetchLocal}});
  let text='';let caught;
  try {
    for await (const chunk of await llm.stream([new HumanMessage(failure.fixture)]))
      text+=typeof chunk.content==='string'?chunk.content:chunk.content.filter(p=>p.type==='text').map(p=>p.text).join('');
  } catch (error) { caught=error; }
  check(caught,'Workflow stream raises instead of ending silently');
  check(caught.status===(failure.status??undefined)&&caught.code===failure.code,`${caught.status} ${caught.code}`);
  check(failure.status!==null||text==='partial ','Partial output precedes the mid-stream failure');
  console.log(`PASS: ${checks} n8n LangChain streamed failure checks`);
  process.exit(0);
}
for (const responses of [false,true]) {
  const llm=new ChatOpenAI({model,apiKey:'test-key',maxRetries:0,timeout:10000,
    useResponsesApi:responses,configuration:{baseURL:base_url,fetch:fetchLocal}});
  for (const streaming of [false,true]) {
    const invoke=async(model,messages)=>{
      if (!streaming) return model.invoke(messages);
      let result;
      for await (const chunk of await model.stream(messages)) result=result?result.concat(chunk):chunk;
      return result;
    };
    const first=await invoke(llm,[new HumanMessage('hello')]);
    const content=typeof first.content==='string'?first.content:first.content.filter(p=>p.type==='text').map(p=>p.text).join('');
    check(content==='Hello from fixture','Workflow text and streaming');
    // n8n's HTTP Request/Wikipedia/Code tools are client-owned functions.
    const schema={$schema:'http://json-schema.org/draft-07/schema#',type:'object',
      properties:{q:{type:'string',minLength:1}},required:['q'],additionalProperties:false};
    const definitions=['lookup','HTTP_Request','Wikipedia','Code_Tool'].map(name=>({
      type:'function',function:{name,description:name,parameters:schema},
    }));
    const tools=llm.bindTools(definitions);
    const history=[new HumanMessage('tools-fixture')];
    const selected=await invoke(tools,history);
    check(selected.tool_calls.length===2,'Workflow parallel handoff');
    check(selected.tool_calls.every(c=>c.name==='lookup'&&c.id),'Exact public names and call IDs');
    check(JSON.stringify(selected.tool_calls.map(c=>c.args))===JSON.stringify([{q:'one'},{q:'two'}]),'Parsed arguments');
    // Workflow-tool/subagent results are opaque data, returned as ToolMessages.
    const last=await invoke(tools,[...history,selected,...selected.tool_calls.map(c=>new ToolMessage({
      tool_call_id:c.id,name:c.name,content:'found',
    }))]);
    const answer=typeof last.content==='string'?last.content:last.content.filter(p=>p.type==='text').map(p=>p.text).join('');
    check(answer==='tool results accepted','Workflow tool-result continuation');
  }
  const structured=llm.withStructuredOutput({type:'object',properties:{answer:{type:'string'}},required:['answer'],additionalProperties:false},
    {method:'jsonSchema',name:'answer'});
  check((await structured.invoke('schema-fixture')).answer==='valid','Workflow structured JSON output');
  check(wire.every(body=>responses?Array.isArray(body.input):Array.isArray(body.messages)),'Expected API wire format');
  wire=[];
}
console.log(`PASS: ${checks} n8n LangChain workflow-agent checks`);
