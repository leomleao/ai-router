#!/usr/bin/env python3
"""Explicit authenticated acceptance checks; never used by test-local.sh.

Run in the local SDK test image. The caller provides one JSON configuration
line on stdin; the temporary gateway key stays out of files and command args.
No AGY auth volume is mounted in this consumer container.
"""
import json
import sys
import time

from openai import OpenAI

passed_checks = []


def check(condition, label):
    if not condition:
        raise AssertionError(label)
    passed_checks.append(label)
    print(json.dumps({"check": label, "passed": True}), flush=True)


def run(configuration):
    client = OpenAI(
        api_key=configuration["api_key"],
        base_url=configuration["base_url"],
        timeout=135,
        max_retries=0,
    )
    models = [model.id for model in client.models.list().data]
    check(bool(models), "Authenticated model discovery")
    model = configuration.get("model") or next(
        (identifier for identifier in models if "flash-low" in identifier), models[0]
    )
    check(model in models, "Selected model exists")

    answer = client.chat.completions.create(
        model=model, messages=[{"role": "user", "content": "Reply exactly ROUTER_OK"}]
    )
    check(answer.choices[0].message.content.strip() == "ROUTER_OK", "Live Chat Completions")
    answer = client.responses.create(model=model, input="Reply exactly RESPONSES_OK", store=False)
    check(answer.output_text.strip() == "RESPONSES_OK", "Live Responses")

    text = ""
    terminal = False
    with client.chat.completions.create(
        model=model, messages=[{"role": "user", "content": "Reply exactly STREAM_OK"}], stream=True
    ) as stream:
        for chunk in stream:
            for choice in chunk.choices:
                text += choice.delta.content or ""
                terminal |= choice.finish_reason == "stop"
    check(text.strip() == "STREAM_OK" and terminal, "Live Chat SSE content and termination")

    text = ""
    terminal = False
    with client.responses.create(model=model, input="Reply exactly RESPONSES_STREAM_OK", store=False, stream=True) as stream:
        for event in stream:
            if event.type == "response.output_text.delta":
                text += event.delta
            elif event.type == "response.completed":
                terminal = event.response.status == "completed"
            elif event.type in {"error", "response.failed"}:
                raise AssertionError("Responses stream failed")
    check(text.strip() == "RESPONSES_STREAM_OK" and terminal, "Live Responses SSE content and termination")

    schema = {"type": "object", "properties": {"answer": {"type": "integer"}}, "required": ["answer"], "additionalProperties": False}
    answer = client.chat.completions.create(
        model=model,
        messages=[{"role": "user", "content": "Return answer equal to 3 plus 4."}],
        response_format={"type": "json_schema", "json_schema": {"name": "arithmetic", "strict": True, "schema": schema}},
    )
    check(json.loads(answer.choices[0].message.content) == {"answer": 7}, "Live independently validated JSON Schema")
    answer = client.responses.create(
        model=model,
        input="Return answer equal to 3 plus 4.",
        text={"format": {"type": "json_schema", "name": "arithmetic", "strict": True, "schema": schema}},
        store=False,
    )
    check(json.loads(answer.output_text) == {"answer": 7}, "Live Responses JSON Schema")

    parameters = {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"], "additionalProperties": False}
    prompt = "Use get_weather to retrieve London's weather. After receiving its result, reply exactly WEATHER_OK."
    tools = [{"type": "function", "function": {"name": "get_weather", "description": "Retrieve current weather for a city", "parameters": parameters, "strict": True}}]
    history = [{"role": "user", "content": prompt}]
    answer = client.chat.completions.create(model=model, messages=history, tools=tools)
    calls = answer.choices[0].message.tool_calls or []
    check(len(calls) == 1 and calls[0].function.name == "get_weather", "Live Chat tool handoff")
    call = calls[0]
    check(bool(call.id) and json.loads(call.function.arguments)["city"].lower() == "london", "Chat call ID and arguments")
    history.extend([
        {"role": "assistant", "content": answer.choices[0].message.content, "tool_calls": [{"id": call.id, "type": "function", "function": {"name": call.function.name, "arguments": call.function.arguments}}]},
        {"role": "tool", "tool_call_id": call.id, "content": json.dumps({"city": "London", "temperature_c": 18})},
    ])
    answer = client.chat.completions.create(model=model, messages=history, tools=tools)
    check(answer.choices[0].message.content.strip() == "WEATHER_OK", "Live Chat client-owned tool result round trip")

    response_tools = [{"type": "function", "name": "get_weather", "description": "Retrieve current weather for a city", "parameters": parameters, "strict": True}]
    answer = client.responses.create(model=model, input=prompt, tools=response_tools, store=False)
    calls = [item for item in answer.output if item.type == "function_call"]
    check(len(calls) == 1 and calls[0].name == "get_weather", "Live Responses tool handoff")
    call = calls[0]
    check(bool(call.call_id) and json.loads(call.arguments)["city"].lower() == "london", "Responses call ID and arguments")
    answer = client.responses.create(
        model=model,
        input=[
            {"role": "user", "content": prompt},
            {"type": "function_call", "call_id": call.call_id, "name": call.name, "arguments": call.arguments},
            {"type": "function_call_output", "call_id": call.call_id, "output": json.dumps({"city": "London", "temperature_c": 18})},
        ],
        tools=response_tools,
        store=False,
    )
    check(answer.output_text.strip() == "WEATHER_OK", "Live Responses client-owned tool result round trip")
    with client.chat.completions.create(
        model=model,
        messages=[{"role": "user", "content": "Count from 1 to 10000, one number per line."}],
        stream=True,
    ) as stream:
        first_chunk = next(iter(stream))
        time.sleep(1)
        check(bool(first_chunk.id), "Live client disconnect during provider startup")
    # The coordinator separately checks monitor cancellation and empty workspaces.
    print(json.dumps({"result": "passed", "checks": len(passed_checks), "model": model, "sdk": "3.24.0"}), flush=True)


if __name__ == "__main__":
    run(json.loads(sys.stdin.readline()))
