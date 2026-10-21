#!/usr/bin/env python3
"""Pinned official SDK against a real TCP gateway and synthetic direct backend."""
import json
import openai
import sys
from openai import OpenAI, APIStatusError

configuration = json.load(sys.stdin)
assert configuration['base_url'].startswith('http://127.0.0.1:')
client = OpenAI(base_url=configuration['base_url'], api_key='test-key', max_retries=0, timeout=10)
model = configuration.get('model', 'gemini-4-argon-medium')
checks = 0

def check(condition, message="contract failed"):
    global checks
    assert condition, message
    checks += 1

# Streamed provider failures must raise, not end quietly: HTTP errors before
# provider acceptance and the top-level stream `error` object afterwards.
failure = configuration.get('failure')
if failure:
    text = ''
    try:
        if failure['api'] == 'responses':
            for event in client.responses.create(model=model, input=failure['fixture'], stream=True, store=False):
                text += event.delta if event.type == 'response.output_text.delta' else ''
        else:
            for chunk in client.chat.completions.create(model=model, messages=[{'role':'user','content':failure['fixture']}], stream=True):
                text += ''.join(choice.delta.content or '' for choice in chunk.choices)
        raise AssertionError('streamed failure ended without an exception')
    except APIStatusError as error:
        check(failure['status'] == error.status_code and failure['code'] == error.code, f'{error.status_code} {error.code}')
    except openai.APIError as error:
        check(failure['status'] is None and failure['code'] == error.code and text == 'partial ', f'{error.code} {text!r}')
    print(f'Official SDK streamed failure raised: {checks} checks')
    sys.exit(0)

check(model in [m.id for m in client.models.list()])
for stream in (False, True):
    chat = client.chat.completions.create(model=model, messages=[{'role':'user','content':'hello'}], stream=stream)
    if stream:
        chunks=list(chat)
        check(len({c.id for c in chunks})==1)
        check(''.join(c.choices[0].delta.content or '' for c in chunks if c.choices) == 'Hello from fixture')
    else:
        check(chat.choices[0].message.content == 'Hello from fixture')
    response = client.responses.create(model=model, input='hello', stream=stream, store=False)
    if stream:
        events = list(response)
        check(''.join(e.delta for e in events if e.type == 'response.output_text.delta') == 'Hello from fixture')
        check(events[0].type=='response.created' and events[-1].type == 'response.completed')
        check([e.sequence_number for e in events]==list(range(len(events))))
    else:
        check(response.output_text == 'Hello from fixture')

schema = {'type':'object','properties':{'answer':{'type':'string'}},'required':['answer'],'additionalProperties':False}
for stream in (False,True):
    chat = client.chat.completions.create(model=model, messages=[{'role':'user','content':'schema-fixture'}], stream=stream,
        response_format={'type':'json_schema','json_schema':{'name':'answer','strict':True,'schema':schema}})
    text = ''.join(c.choices[0].delta.content or '' for c in chat if c.choices) if stream else chat.choices[0].message.content
    check(json.loads(text) == {'answer':'valid'})
    response = client.responses.create(model=model, input='schema-fixture', stream=stream, store=False,
        text={'format':{'type':'json_schema','name':'answer','strict':True,'schema':schema}})
    text = ''.join(e.delta for e in response if e.type == 'response.output_text.delta') if stream else response.output_text
    check(json.loads(text) == {'answer':'valid'})

tool = {'type':'function','function':{'name':'lookup','description':'Lookup','parameters':{'type':'object','properties':{'q':{'type':'string'}},'required':['q'],'additionalProperties':False}}}
first = client.chat.completions.create(model=model,messages=[{'role':'user','content':'tools-fixture'}],tools=[tool])
calls = first.choices[0].message.tool_calls
check(len(calls)==2)
check([json.loads(c.function.arguments) for c in calls] == [{'q':'one'},{'q':'two'}])
history=[{'role':'user','content':'tools-fixture'},first.choices[0].message.model_dump(exclude_none=True)]
history += [{'role':'tool','tool_call_id':c.id,'content':'found'} for c in calls]
last=client.chat.completions.create(model=model,messages=history,tools=[tool])
check(last.choices[0].message.content=='tool results accepted')
none=client.chat.completions.create(model=model,messages=[{'role':'user','content':'tools-fixture'}],tools=[tool],tool_choice='none')
check(not none.choices[0].message.tool_calls)
response_tool = {'type':'function',**tool['function']}
first = client.responses.create(model=model,input='tools-fixture',tools=[response_tool])
response_calls=[item for item in first.output if item.type=='function_call']
check(len(response_calls)==2)
follow=[{'role':'user','content':'tools-fixture'}]+[item.model_dump(exclude_none=True) for item in first.output]
follow += [{'type':'function_call_output','call_id':item.call_id,'output':'found'} for item in response_calls]
last=client.responses.create(model=model,input=follow,tools=[response_tool],stream=True)
check(''.join(item.delta for item in last if item.type=='response.output_text.delta')=='tool results accepted')
for endpoint in ('agy/runs','agy/runs/x/events','embeddings','assistants','threads','threads/x/runs'):
    try:
        client.post('/'+endpoint,body={},cast_to=dict)
        raise AssertionError('removed endpoint accepted')
    except APIStatusError as error:
        check(error.status_code==404)

# Official SDK tool handoff coverage complements the pinned Vercel clients:
# stream/nonstream first turns and continuations on both public APIs.
rich_schema={'$schema':'http://json-schema.org/draft-07/schema#','type':'object',
    '$defs':{'Query':{'type':'string','minLength':1}},
    'properties':{'q':{'$ref':'#/$defs/Query'}},'required':['q'],'additionalProperties':False}
for response_api in (False, True):
    for streaming in (False, True):
        tools=[{'type':'function', 'name':'lookup','description':'Lookup','parameters':rich_schema}] if response_api else [
            {'type':'function','function':{'name':'lookup','description':'Lookup','parameters':rich_schema}}]
        if response_api:
            initial=client.responses.create(model=model,input='tools-fixture',tools=tools,stream=streaming)
            if streaming:
                events=list(initial)
                check(events[-1].type=='response.completed')
                initial=events[-1].response
            calls=[item for item in initial.output if item.type=='function_call']
            check(len(calls)==2)
            check([json.loads(c.arguments) for c in calls]==[{'q':'one'},{'q':'two'}])
            # A workflow may continue via stored item references rather than
            # replaying the assistant messages used by another n8n consumer.
            follow=[{'role':'user','content':'tools-fixture'}]
            follow += [{'type':'item_reference','id':item.id} for item in initial.output]
            follow += [{'type':'function_call_output','call_id':c.call_id,'output':'found'} for c in reversed(calls)]
            final=client.responses.create(model=model,input=follow,tools=tools,stream=not streaming)
            text=''.join(e.delta for e in final if e.type=='response.output_text.delta') if not streaming else final.output_text
        else:
            history=[{'role':'user','content':'tools-fixture'}]
            initial=client.chat.completions.create(model=model,messages=history,tools=tools,stream=streaming)
            if streaming:
                accumulated={}
                for chunk in initial:
                    for choice in chunk.choices:
                        for call in choice.delta.tool_calls or []:
                            value=accumulated.setdefault(call.index,{'id':None,'type':'function','function':{'name':'','arguments':''}})
                            if call.id: value['id']=call.id
                            if call.function:
                                value['function']['name']+=call.function.name or ''
                                value['function']['arguments']+=call.function.arguments or ''
                calls=[accumulated[i] for i in sorted(accumulated)]
                assistant={'role':'assistant','tool_calls':calls}
            else:
                assistant=initial.choices[0].message.model_dump(exclude_none=True)
                calls=assistant['tool_calls']
            check(len(calls)==2)
            check([json.loads(c['function']['arguments']) for c in calls]==[{'q':'one'},{'q':'two'}])
            history += [assistant]+[{'role':'tool','tool_call_id':c['id'],'content':'found'} for c in reversed(calls)]
            final=client.chat.completions.create(model=model,messages=history,tools=tools,stream=not streaming)
            text=''.join(c.choices[0].delta.content or '' for c in final if c.choices) if not streaming else final.choices[0].message.content
        check(text=='tool results accepted')
def token_budget_contracts():
    client = OpenAI(api_key='test-key', base_url=configuration['base_url'], max_retries=0, timeout=8)
    budget = client.get('/capabilities',cast_to=dict)["profiles"]["model"]["output_token_budget"]
    check(budget == {"mode": "prompt_guidance", "hard_limit": False,
          "limits_reasoning_tokens": False, "parameters": ["max_output_tokens", "max_tokens", "max_completion_tokens"]},
          "Capabilities describe approximate visible-answer guidance without a reasoning/spending cap")
    for field in ("max_tokens", "max_completion_tokens", "max_output_tokens"):
        responses = field == "max_output_tokens"
        create = client.responses.with_raw_response.create if responses else client.chat.completions.with_raw_response.create
        base = ({"model": model, "input": "Reply with OK.", "store": False,
                 "text": {"format": {"type": "text"}, "verbosity": "medium"}} if responses else
                {"model": model, "messages": [{"role": "user", "content": "Reply with OK."}], "verbosity": "medium"})
        for value in (1, 8, 16):
            raw = create(**base, **{field: value})
            result = raw.parse()
            check(raw.headers.get("x-ai-router-token-budget-mode") == "prompt-guidance", f"{field}={value} advertises best-effort mode")
            text = result.output_text if responses else result.choices[0].message.content
            check(text == "Hello from fixture" and result.usage.total_tokens == 10, f"{field}={value} preserves full output and observed usage")
            check(result.status == "completed" if responses else result.choices[0].finish_reason == "stop",
                  f"{field}={value} does not fabricate length/incomplete completion")
            if value == 1:
                observed_output = result.usage.output_tokens if responses else result.usage.completion_tokens
                check(observed_output > value, f"{field}: observed usage may exceed the length hint")

        # Budget-bearing streaming and strict schema requests retain their
        # existing wire contract and never truncate JSON to satisfy guidance.
        raw = create(**base, **{field: 8, "stream": True}, **({} if responses else {"stream_options": {"include_usage": True}}))
        events = list(raw.parse())
        check(raw.headers.get("x-ai-router-token-budget-mode") == "prompt-guidance", f"{field} SSE advertises best-effort mode")
        if responses:
            final = events[-1].response
            check(events[-1].type == "response.completed" and final.status == "completed" and final.usage.total_tokens == 10,
                  "Budgeted Responses stream preserves successful terminal usage")
            check("".join(event.delta for event in events if event.type == "response.output_text.delta") == "Hello from fixture",
                  "Budgeted Responses stream preserves every text delta")
            format_options = {"text": {"format": {"type": "json_schema", "name": "answer", "schema": schema, "strict": True}, "verbosity": "medium"}}
        else:
            check(events[-1].usage.total_tokens == 10 and any(chunk.choices and chunk.choices[0].finish_reason == "stop" for chunk in events),
                  f"{field} stream retains observed usage and stop reason")
            check("".join(chunk.choices[0].delta.content or "" for chunk in events if chunk.choices) == "Hello from fixture",
                  f"{field} stream preserves every text delta")
            format_options = {"response_format": {"type": "json_schema", "json_schema": {"name": "answer", "schema": schema, "strict": True}}}
        schema_base=base | ({'input':'schema-fixture'} if responses else {'messages':[{'role':'user','content':'schema-fixture'}]})
        raw = create(**(schema_base | format_options | {field: 16}))
        result = raw.parse()
        text = result.output_text if responses else result.choices[0].message.content
        check(json.loads(text) == {'answer':'valid'} and result.usage.total_tokens == 10,
              f"{field} guidance preserves valid strict JSON and usage")
        check(raw.headers.get("x-ai-router-token-budget-mode") == "prompt-guidance", f"{field} schema advertises best-effort mode")

        raw = create(**base, **{field: None})
        check(raw.headers.get("x-ai-router-token-budget-mode") is None, f"Null {field} remains equivalent to omission")
        for malformed in (0, -1, 1.0, 1.5, "8", True, [], {}):
            try:
                create(**base, **{field: malformed})
                raise AssertionError(f"Malformed {field} accepted: {malformed!r}")
            except openai.BadRequestError as error:
                check(error.status_code == 400 and error.code == "invalid_request", f"Malformed {field} yields SDK BadRequestError")

    result = client.responses.create(model=model, input="Reply with OK.", store=False, max_output_tokens=1)
    check(result.usage.output_tokens == 6 and result.status == "completed" and result.output_text == "Hello from fixture",
          "One-token guidance does not clamp actual six-token fixture usage or truncate output")
    try:
        client.chat.completions.create(model=model, messages=[{"role": "user", "content": "Reply with OK."}],
                                       max_tokens=8, max_completion_tokens=16)
        raise AssertionError("Conflicting Chat budgets accepted")
    except openai.BadRequestError as error:
        check(error.status_code == 400 and error.code == "invalid_request", "Non-null Chat budget aliases are mutually exclusive")
    client.close()


# Features intentionally outside the text/client-function contract must be
# rejected consistently instead of silently altering the n8n request.
for response_api in (False,True):
    create=client.responses.create if response_api else client.chat.completions.create
    base={'model':model,'input':'hello'} if response_api else {'model':model,'messages':[{'role':'user','content':'hello'}]}
    for options in ({'tool_choice':{'type':'custom','name':'lookup'},'tools':[response_tool] if response_api else [tool]},
                    {'tools':[{'type':'web_search'}]}, {'extra_body':{'unknown_control':True}},
                    {'extra_body':{'logit_bias':{'1':1}}}, {'temperature':2.5}):
        try:
            create(**base,**options)
            raise AssertionError('unsupported request accepted')
        except APIStatusError as error:
            check(error.status_code==400 and error.response.headers.get('x-should-retry')=='false')
    # Sampling hints are range-checked, ignored and disclosed, never enforced.
    raw=(client.responses if response_api else client.chat.completions).with_raw_response.create(
        **base,temperature=0.5,top_p=1,metadata={'workflow':'synthetic'},user='user-123')
    result=raw.parse()
    check((result.output_text if response_api else result.choices[0].message.content)=='Hello from fixture')
    check(raw.headers.get('x-ai-router-sampling-controls')=='ignored', 'Ignored sampling hints are disclosed')
    check(raw.headers.get('x-ai-router-ignored-fields')=='metadata, user', 'Ignored request fields are named')
    if response_api:
        check(result.metadata=={'workflow':'synthetic'}, 'Responses echo request metadata')
    for verbosity in ('low','high'):
        options={'text':{'verbosity':verbosity}} if response_api else {'extra_body':{'verbosity':verbosity}}
        try:
            create(**base,**options)
            raise AssertionError('unsupported verbosity accepted')
        except APIStatusError as error:
            check(error.status_code==400)

# Forced tool choice narrows the inventory and requires a call. The fixture
# model obeys the router's instruction; a text-only answer fails closed.
for response_api in (False,True):
    tools=[response_tool] if response_api else [tool]
    if response_api:
        forced=client.responses.create(model=model,input='hello',tools=tools,tool_choice='required')
        check([item.name for item in forced.output if item.type=='function_call']==['lookup'])
        named={'type':'function','name':'missing'}
    else:
        forced=client.chat.completions.create(model=model,messages=[{'role':'user','content':'hello'}],tools=tools,
            tool_choice={'type':'function','function':{'name':'lookup'}},parallel_tool_calls=False)
        check([call.function.name for call in forced.choices[0].message.tool_calls]==['lookup'])
        named={'type':'function','function':{'name':'missing'}}
    for options,status,code in (({'tool_choice':'required'},502,'missing_required_tool_call'),
                                ({'tool_choice':named},400,'invalid_request')):
        base={'model':model,'input':'no-call-fixture'} if response_api else {'model':model,'messages':[{'role':'user','content':'no-call-fixture'}]}
        try:
            (client.responses.create if response_api else client.chat.completions.create)(**base,tools=tools,**options)
            raise AssertionError('forced tool choice completed without a call')
        except APIStatusError as error:
            check(error.status_code==status and error.code==code)
            check(status!=400 or error.param=='tool_choice')
with client.responses.stream(model=model,input='hello',store=False) as stream:
    list(stream)
    check(stream.get_final_response().output_text=='Hello from fixture')
token_budget_contracts()
print(f'Expanded official SDK matrix passed: {checks} checks')
