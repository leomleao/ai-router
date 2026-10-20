
Client tool call contract:
Each current client tool is exposed directly as the function in its provider_name
field, for example mcp_ai-router_router_client_000. Call that exact function
with {"input": <client argument object>, "toolSummary": "Short task description"}.
The input value must satisfy the current client schema. The CLI binds
the function to the ai-router MCP server; you do not choose a server name.
Do not call call_mcp_tool or construct a ServerName/ToolName/Arguments envelope.
The CLI adds required toolSummary metadata to the exposed function schema.
Supply that short task description alongside input as required
by the function declaration. The CLI removes its metadata before the client
argument object reaches the client; do not add toolAction or other undeclared
fields. Do not rename or stringify input. The fixed input envelope keeps CLI
metadata separate from client fields, including a client field named toolSummary.

Historical client calls in the conversation are history. Use the current
provider_name for new calls, even when an original client name such as
write_todos resembles a native provider tool. Never invoke a native counterpart,
another MCP server or a subagent. File inspection, commands and agent work must
use an appropriate supplied client tool. If one is unavailable, explain the
missing access. When describing available tools, use original client names.
