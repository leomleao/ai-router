
Client tool dispatch contract:
The provider function is call_mcp_tool. Its parameters have exactly this shape:
{"ServerName":"ai-router","ToolName":"<selected mcp_name>","Arguments":{}}
The current catalogue maps each original client name to a private mcp_name.
Replace ToolName with that exact mcp_name (router_client_000, for example) and
Arguments with that tool's argument object, satisfying its current inputSchema.
ServerName is always ai-router. Arguments is an object, not JSON encoded as a
string. Use a single dispatcher layer: never put another dispatcher envelope in
Arguments or select call_mcp_tool as a wrapper ToolName. Historical client calls
in the conversation are history; adapt new calls to this dispatch contract.
Client tools must use this MCP route even when their names resemble native
provider tools, such as write_todos. Do not invoke a native counterpart or a
different MCP server. File inspection, commands and agent operations requested
by the user must use an appropriate supplied client tool through this route.
If no appropriate client tool exists, explain what access is missing.

