#!/usr/bin/env python3
"""Pinned OpenAI SDK media and hosted-search contract against a synthetic loopback gateway."""
import base64
import json
import sys
from urllib.parse import urlsplit

import httpx2 as httpx
import openai
from openai import OpenAI


configuration = json.load(sys.stdin)
base_url = configuration["base_url"]
parsed_url = urlsplit(base_url)
assert parsed_url.scheme == "http" and parsed_url.hostname == "127.0.0.1" and parsed_url.port
assert openai.__version__ == "3.24.0", f"unexpected OpenAI SDK: {openai.__version__}"
model = configuration["model"]
key = "test-key"
checks = 0
wire = []


def check(condition, message="media/search contract failed"):
    global checks
    assert condition, message
    checks += 1


def record_request(request):
    assert request.url.scheme == "http" and request.url.host == "127.0.0.1"
    assert request.url.port == parsed_url.port
    wire.append({"path": request.url.path, "body": json.loads(request.content)})


http_client = httpx.Client(
    timeout=httpx.Timeout(10.0),
    trust_env=False,
    follow_redirects=False,
    event_hooks={"request": [record_request]},
)
client = OpenAI(
    base_url=base_url,
    api_key=key,
    max_retries=0,
    timeout=10.0,
    http_client=http_client,
)

# These fixtures are public, synthetic bytes only. The router checks media
# signatures; its mock upstream does not fetch or extract uploaded content.
PNG = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/T9sAAAAASUVORK5CYII="
PDF_BYTES = b"%PDF-1.4\n1 0 obj<</Type/Catalog>>endobj\n%%EOF"
PDF = "data:application/pdf;base64," + base64.b64encode(PDF_BYTES).decode("ascii")
TEXT_BYTES = b"synthetic text fixture"
TEXT_FILE = "data:text/plain;base64," + base64.b64encode(TEXT_BYTES).decode("ascii")


def one_request(path, expected_body):
    check(len(wire) == 1, "one SDK request with maxRetries=0")
    request = wire.pop()
    check(request["path"] == path, f"expected {path}, got {request['path']}")
    check(expected_body(request["body"]), f"request media/search mapping: {request['body']!r}")


def check_response_search(result):
    check(result.output_text == "Search fixture answer", "Responses search answer")
    items = [item for item in result.output if item.type == "web_search_call"]
    check(len(items) == 1, "completed hosted web_search_call survives Responses decoding")
    check(not any(item.type == "function_call" for item in result.output),
          "hosted search does not become a custom function handoff")
    search = items[0]
    check(search.status == "completed" and search.action.type == "search", "completed search action")
    sources = search.action.sources
    check(any(source.url == "https://example.test/source" for source in sources), "hosted search source URL")
    annotations = [
        annotation
        for item in result.output
        if item.type == "message"
        for content in item.content
        if content.type == "output_text"
        for annotation in content.annotations
        if annotation.type == "url_citation"
    ]
    check(
        any(
            citation.url == "https://example.test/source"
            and citation.title == "Fixture source"
            and citation.start_index == 0
            and citation.end_index == 6
            for citation in annotations
        ),
        "flat Responses URL citation survives with exact source, title and indexes",
    )


def field(value, name):
    return value.get(name) if isinstance(value, dict) else getattr(value, name, None)


def check_chat_search(message, finish_reason):
    check(field(message, "content") == "Search fixture answer", "Chat search answer")
    annotations = field(message, "annotations") or []
    check(
        any(
            field(annotation, "type") == "url_citation"
            and field(field(annotation, "url_citation"), "url") == "https://example.test/source"
            and field(field(annotation, "url_citation"), "title") == "Fixture source"
            and field(field(annotation, "url_citation"), "start_index") == 0
            and field(field(annotation, "url_citation"), "end_index") == 6
            for annotation in annotations
        ),
        "public Chat nested URL citation survives with exact source, title and indexes",
    )
    check(not field(message, "tool_calls") and finish_reason != "tool_calls",
          "hosted search is not a custom function handoff")


# Chat Completions image_url and Responses input_image, in both SDK response
# modes. The wire assertions catch client serialization regressions as well as
# gateway response compatibility.
for streaming in (False, True):
    wire.clear()
    messages = [{"role": "user", "content": [
        {"type": "text", "text": "Describe this synthetic image."},
        {"type": "image_url", "image_url": {"url": f"data:image/png;base64,{PNG}"}},
    ]}]
    response = client.chat.completions.create(model=model, messages=messages, stream=streaming)
    if streaming:
        text = ""
        finish = None
        for chunk in response:
            for choice in chunk.choices:
                text += choice.delta.content or ""
                if choice.finish_reason:
                    finish = choice.finish_reason
        check(text == "OpenAI synthetic answer", "streamed Chat image answer")
        check(finish == "stop", "streamed Chat image stop")
    else:
        message = response.choices[0].message
        check(message.content == "OpenAI synthetic answer", "Chat image answer")
    one_request(
        "/v1/chat/completions",
        lambda body: body["messages"][0]["content"][1]["type"] == "image_url"
        and body["messages"][0]["content"][1]["image_url"]["url"] == f"data:image/png;base64,{PNG}",
    )

    wire.clear()
    response_input = [{"role": "user", "content": [
        {"type": "input_text", "text": "Describe this synthetic image."},
        {"type": "input_image", "image_url": f"data:image/png;base64,{PNG}"},
    ]}]
    response = client.responses.create(model=model, input=response_input, stream=streaming, store=False)
    if streaming:
        text = ""
        final = None
        for event in response:
            if event.type == "response.output_text.delta":
                text += event.delta
            elif event.type == "response.completed":
                final = event.response
        check(text == "OpenAI synthetic answer", "streamed Responses image answer")
        check(final is not None and final.status == "completed", "streamed Responses image completed")
    else:
        check(response.output_text == "OpenAI synthetic answer", "Responses image answer")
    one_request(
        "/v1/responses",
        lambda body: any(
            part.get("type") == "input_image" and part.get("image_url") == f"data:image/png;base64,{PNG}"
            for item in body["input"] if isinstance(item, dict) and item.get("type", "message") == "message"
            for part in item.get("content", [])
        ),
    )

# Chat's standard file part and Responses input_file support. Both are sent to
# the synthetic loopback gateway; no external file URL is used.
wire.clear()
chat_file = client.chat.completions.create(
    model=model,
    messages=[{"role": "user", "content": [
        {"type": "text", "text": "Read this synthetic PDF."},
        {"type": "file", "file": {"filename": "fixture.pdf", "file_data": PDF}},
    ]}],
)
check(chat_file.choices[0].message.content == "OpenAI synthetic answer", "Chat PDF answer")
one_request(
    "/v1/chat/completions",
    lambda body: any(
        part.get("type") == "file" and part.get("file", {}).get("filename") == "fixture.pdf"
        and part["file"].get("file_data") == PDF
        for part in body["messages"][0]["content"]
    ),
)

for filename, file_data, media_type in (("fixture.pdf", PDF, "application/pdf"), ("fixture.txt", TEXT_FILE, "text/plain")):
    wire.clear()
    response = client.responses.create(
        model=model,
        input=[{"role": "user", "content": [
            {"type": "input_text", "text": "Read this synthetic file."},
            {"type": "input_file", "filename": filename, "file_data": file_data},
        ]}],
        store=False,
    )
    check(response.output_text == "OpenAI synthetic answer", f"Responses {media_type} answer")
    one_request(
        "/v1/responses",
        lambda body, filename=filename, file_data=file_data: any(
            part.get("type") == "input_file" and part.get("filename") == filename
            and part.get("file_data") == file_data
            for item in body["input"] if isinstance(item, dict) and item.get("type", "message") == "message"
            for part in item.get("content", [])
        ),
    )

# Hosted search uses the standard web_search tool. Chat's extension carries
# the include hint through extra_body; Responses uses its first-party include.
for streaming in (False, True):
    wire.clear()
    chat_tools = [{"type": "web_search"}]
    chat = client.chat.completions.create(
        model=model,
        messages=[{"role": "user", "content": "web_search contract fixture"}],
        tools=chat_tools,
        stream=streaming,
        extra_body={"include": ["web_search_call.action.sources"]},
    )
    if streaming:
        text = ""
        annotations = []
        finish = None
        tool_calls = []
        for chunk in chat:
            for choice in chunk.choices:
                text += choice.delta.content or ""
                annotations.extend(choice.delta.model_dump().get("annotations") or [])
                tool_calls.extend(choice.delta.tool_calls or [])
                if choice.finish_reason:
                    finish = choice.finish_reason
        # Streaming Chat sends the same nested annotation shape on deltas.
        message_view = {"content": text, "tool_calls": tool_calls, "annotations": annotations}
        check_chat_search(message_view, finish)
    else:
        check_chat_search(chat.choices[0].message, chat.choices[0].finish_reason)
    one_request(
        "/v1/chat/completions",
        lambda body: body.get("tools") == [{"type": "web_search"}]
        and body.get("include") == ["web_search_call.action.sources"],
    )

    wire.clear()
    responses = client.responses.create(
        model=model,
        input="web_search contract fixture",
        tools=[{"type": "web_search"}],
        include=["web_search_call.action.sources"],
        stream=streaming,
        store=False,
    )
    if streaming:
        final = None
        text = ""
        for event in responses:
            if event.type == "response.output_text.delta":
                text += event.delta
            elif event.type == "response.completed":
                final = event.response
        check(text == "Search fixture answer", "streamed Responses hosted-search answer")
        check(final is not None and final.status == "completed", "streamed Responses search completed")
        check_response_search(final)
    else:
        check_response_search(responses)
    one_request(
        "/v1/responses",
        lambda body: body.get("tools") == [{"type": "web_search"}]
        and body.get("include") == ["web_search_call.action.sources"],
    )

client.close()
print(f"PASS: {checks} OpenAI 3.24.0 media and hosted-search checks")
