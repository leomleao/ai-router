#!/usr/bin/env python3
"""Pinned official SDK against a real TCP gateway and synthetic direct backend."""
import json
import sys
from openai import OpenAI, APIStatusError

configuration = json.load(sys.stdin)
assert configuration['base_url'].startswith('http://127.0.0.1:')
client = OpenAI(base_url=configuration['base_url'], api_key='test-key', max_retries=0, timeout=10)
model = configuration.get('model', 'gemini-4-argon-medium')
checks = 0

def check(condition):
    global checks
    assert condition
    checks += 1

check(model in [m.id for m in client.models.list()])
for stream in (False, True):
    chat = client.chat.completions.create(model=model, messages=[{'role':'user','content':'hello'}], stream=stream)
    if stream:
        check(''.join(c.choices[0].delta.content or '' for c in chat if c.choices) == 'Hello from fixture')
    else:
        check(chat.choices[0].message.content == 'Hello from fixture')
    response = client.responses.create(model=model, input='hello', stream=stream, store=False)
    if stream:
        events = list(response)
        check(''.join(e.delta for e in events if e.type == 'response.output_text.delta') == 'Hello from fixture')
        check(events[-1].type == 'response.completed')
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
for endpoint in ('agy/runs','agy/runs/x/events','embeddings'):
    try:
        client.post('/'+endpoint,body={},cast_to=dict)
        raise AssertionError('removed endpoint accepted')
    except APIStatusError as error:
        check(error.status_code==404)
print(f'Official OpenAI SDK direct TCP checks passed: {checks}')
