# Parallel implementation contract

All modules live in one Rust crate, `ai-router`, with `src/lib.rs` exporting
`config`, `auth`, `protocol`, `runner`, `relay`, `monitor`, and `api`. Coordinate
interface changes across modules. Never read protected secret files; use
synthetic fixtures for validation.

## P1 owner: config.rs, auth.rs, protocol.rs

`Config::from_env() -> Result<Config, String>` reads environment values directly
(no dotenv). `Config::for_test(root: &Path) -> Config` uses synthetic key digests.
Root wraps config in `Arc<Config>`. Public fields:

```text
bind: String (default 0.0.0.0:8080 inside container)
agy_bin: PathBuf; expected_agy_version: String (1.2.15)
state_dir: PathBuf; workspace_dir: PathBuf; telemetry_dir: PathBuf
monitor_socket: PathBuf; keys: Vec<KeyConfig>
trusted_proxies: Vec<IpAddr>
max_body_bytes: usize (1 MiB); max_output_bytes: usize (8 MiB)
max_concurrent: usize (2); max_queue: usize (8)
request_timeout_secs: u64 (120); artifact_ttl_secs: u64 (3600)
preauth_per_minute: u32 (60); global_preauth_per_minute: u32 (600)
per_key_per_minute: u32 (30); native_enabled: bool (false)
retention_days: u64 (30); max_events: usize (50000)
```

KeyConfig: `id: String`, `sha256: String` (64 hex chars), `scopes: Vec<String>`
(`model`, `native`). `AI_ROUTER_KEYS` contains JSON digest records, never raw
keys. Config validates bounds, absolute storage paths, digest/ID/scope format.
`auth::generate_key(id, scopes) -> (String, KeyConfig)` uses 64 random bytes.
`authenticate(&[KeyConfig], bearer: &str) -> Option<KeyConfig>` uses constant-time
digest comparison. `RateLimiter::new(limit:u32, max_entries:usize)` and
`check(&self, key:&str) -> bool` is a bounded 60-second fixed-window admission gate.

Protocol types derive serde where appropriate:

- `ApiError { status: u16, code: String, message: String }`, constructors and
  Axum IntoResponse: `{"error":{"message":...,"type":...,"code":...}}`.
- `ToolDefinition { name:String, description:String, parameters:Value }`.
- `ToolCall { id:String, name:String, arguments:Value }`.
- `Usage { input_tokens:u64, output_tokens:u64, thinking_tokens:u64,
  cache_read_tokens:u64, total_tokens:u64 }`.
- `InputFile { path:String, data:Vec<u8> }`.
- `RunProfile` enum `Model`, `Native`.
- `RunRequest { request_id:String, model:String, prompt:String,
  system:String, tools:Vec<ToolDefinition>, schema:Option<Value>,
  profile:RunProfile, effort:Option<String>, mode:Option<String>, files:Vec<InputFile> }`.
- `RunResult { text:String, structured_output:Option<Value>,
  conversation_id:String, usage:Option<Usage>, usage_partial:bool,
  tool_calls:Vec<ToolCall>, status:String }`.
- `RunEvent` serde tagged: `Init {conversation_id:String, tools:Vec<String>}`,
  `TextDelta {delta:String}`, `ToolCall(ToolCall)`, `Native(Value)`,
  `Completed(RunResult)`, `Error(ApiError)`.
- `ModelInfo {id:String, name:String}`; `ProviderStatus {version:String,
  authenticated:bool, models:Vec<ModelInfo>, quota:Option<Value>,
  checked_at:String, error:Option<String>}`; Default unavailable.
- `normalize_chat(Value, request_id:String) -> Result<RunRequest,ApiError>` and
  `normalize_response(Value, request_id:String) -> Result<RunRequest,ApiError>`.
  Preserve all supplied messages and tool results in deterministic rendering;
  collect leading system/developer messages into system. Reject unsupported
  modalities/parameters and validate tool names/schemas. Support standard auto
  and none tool choice, disable parallel calls; do not silently ignore forced
  tool choices or sampling controls. Accept benign SDK defaults only with
  documented equivalence. Preserve tool-call IDs/history exactly.

## P2 owner: runner.rs, relay.rs, tests/fixtures/fake-agy (and runner unit tests)

`Runner::new(Arc<Config>) -> Runner` (Clone).
`Runner::start(RunRequest, CancellationToken) -> Result<RunStream, ApiError>`
is async and returns before any filesystem preparation. `RunStream` contains
`events:mpsc::Receiver<RunEvent>`, `workspace:PathBuf`,
`workspace_owned:Arc<AtomicBool>` and `finished:CancellationToken`. An owned
worker prepares the workspace and supervises the process; preparation failures
arrive as terminal error events. `finished` is signalled after filesystem and
process cleanup. Workspace is `config.workspace_dir/<request_id>`; the root
holds admission until cleanup and deletes only directories the runner created.
`Runner::probe() -> ProviderStatus` is async, bounded and never logs credentials.
Cancel/timeout kills the complete child process group, drains/waits, and emits
one terminal event. AGY is invoked by argument vector, never a shell. Capture
bounded stderr privately; sanitised provider error codes go to HTTP/telemetry.
Use real CLI `--input-format stream-json --output-format stream-json`, disable
slash expansion on model requests, and select an excludeDefaultComponents /
inheritMcp:false fixed custom agent. Unknown built-in model tools are fatal.

`relay::serve_stdio(...)` supports MCP initialise, tools/list and tools/call.
Launch relay via the current ai-router executable subcommand `mcp-relay` with
one router-created catalogue path argument. File is synthetic tool definitions,
not credentials. Validate the path stays in workspace. Tools/call is inert:
capture once to a mode-0600 workspace handoff file, never execute or return a
fake tool result. Runner must detect that handoff promptly and return ToolCall /
Completed with partial usage, then kill the process group. Native uses the
official sandbox, never permission skipping, and is gated by config.

## P3 owner: monitor/, Dockerfile, compose.yaml, .dockerignore, scripts/, docs/operations.md

`monitor::Telemetry::open(&Path, retention_days:u64, max_events:usize)
 -> Result<Telemetry,String>`; stores metadata only, bounded retention.
`async record(&self, RequestRecord)`; `async snapshot(&self) -> Snapshot`;
`async set_provider(&self, ProviderStatus)`;
`async set_runtime(&self, active:usize, queued:usize)`.
`note_rejection(&self, code:&str)` synchronously updates six fixed atomic
counters. Preflight rejection history is sampled at 30 records/minute. These
counters reset at process start and are distinct from retained-history totals.
Insertion evicts excess records in O(1); expiration scans run at most once per
minute, and snapshot aggregation runs outside the records mutex.
RequestRecord: `request_id:String, client_id:String, model:String, endpoint:String,
started_at:String, duration_ms:u64, startup_ms:Option<u64>, first_output_ms:Option<u64>,
status:String, error_code:Option<String>, usage:Option<Usage>, usage_partial:bool`.
Snapshot derives Serialize; includes recent records and aggregate status/usage.
`async monitor::serve(Arc<Telemetry>, PathBuf, CancellationToken)
 -> Result<(),String>` implements read-only mode-0600 Unix socket. Protocol:
client writes `snapshot\n`, server writes one JSON snapshot and closes.
`monitor::MonitorArgs { json:bool, text:bool, range_seconds:u64 }` and
`async monitor::client(socket:&Path,args:MonitorArgs) -> Result<(),String>`.

Docker publishes only `127.0.0.1:${AI_ROUTER_PORT:-8090}:8080`; no env_file.
Named auth and telemetry volumes, tmpfs request workspaces. No Docker socket,
host home folders, privileged mode, or dangerous permission flags. Download
official pinned CLI with version verification; document any sandbox runtime
restrictions, do not silently grant full privileges. Scripts never read .env or
credential files. Operator key setup/login are outside Codex. Docker test build
must exclude all banned patterns anywhere in context plus target/.git.

## P4 root integration

Authenticate/limit requests before normalisation/runner. Cache actual provider
probe results; no catalog/quota subprocess per HTTP call. Shared event-to-SSE
renderers with stable IDs and correct finish/error events. Owner-scoped async
native jobs, event replay bounded by max_output_bytes, TTL/maximum job count,
cancel route, safe artifact listing/download. Reject native generation before
sandbox verification and never accept caller-supplied raw provider IDs. Chat
workspaces deleted on completion/cancellation; native workspaces on TTL/delete.
Metrics never include event payloads. Tests use explicit fake provider path and
synthetic keys; fixtures cannot launch signed-in host AGY.
`AppState::drain(Duration) -> bool` waits for admission cleanup and telemetry
recording before shutdown flush. `native::shutdown(&AppState) -> bool` cancels
jobs and reports whether ownership-aware workspace cleanup completed. Failed
or expired cleanup produces a nonzero gateway exit; Compose allows 30 seconds.
