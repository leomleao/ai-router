//! Private, read-only operational telemetry. Payloads never enter this module.
use crate::protocol::{ProviderStatus, Usage};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::io::{IsTerminal, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, Notify, Semaphore};
use tokio_util::sync::CancellationToken;

const MAX_STORE_BYTES: u64 = 64 * 1024 * 1024;
const PRUNE_INTERVAL: Duration = Duration::from_secs(60);

/// Unsampled rejection counters for this process only. Neither the retained
/// history window nor the monitor client's range filter changes these totals.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CountsSinceStart {
    pub started_at: String,
    pub reset_on_restart: bool,
    pub total: u64,
    pub auth: u64,
    pub rate: u64,
    pub scope: u64,
    pub input: u64,
    pub busy: u64,
    pub other: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RequestRecord {
    pub request_id: String,
    pub client_id: String,
    pub model: String,
    pub endpoint: String,
    pub started_at: String,
    pub duration_ms: u64,
    #[serde(default)]
    pub startup_ms: Option<u64>,
    pub first_output_ms: Option<u64>,
    pub status: String,
    pub error_code: Option<String>,
    pub usage: Option<Usage>,
    pub usage_partial: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Aggregate {
    pub requests: usize,
    pub failures: usize,
    pub auth_failures: usize,
    pub complete_usage_records: usize,
    pub partial_usage_records: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub thinking_tokens: u64,
    pub cache_read_tokens: u64,
    pub total_tokens: u64,
    pub duration_p50_ms: Option<u64>,
    pub duration_p95_ms: Option<u64>,
    pub first_output_p50_ms: Option<u64>,
    pub first_output_p95_ms: Option<u64>,
    pub startup_p50_ms: Option<u64>,
    pub startup_p95_ms: Option<u64>,
    pub by_client: BTreeMap<String, usize>,
    pub by_model: BTreeMap<String, usize>,
    pub by_error: BTreeMap<String, usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub generated_at: String,
    pub active: usize,
    pub queued: usize,
    pub provider: ProviderStatus,
    pub provider_age_seconds: Option<u64>,
    pub aggregate: Aggregate,
    pub counts_since_start: CountsSinceStart,
    pub records: Vec<RequestRecord>,
    pub retention_days: u64,
    pub max_events: usize,
    pub persistence_errors: u64,
}

struct State {
    records: VecDeque<RequestRecord>,
    provider: ProviderStatus,
    active: usize,
    queued: usize,
    persistence_errors: u64,
    last_prune: Instant,
}

impl Default for State {
    fn default() -> Self {
        Self {
            records: VecDeque::new(),
            provider: ProviderStatus::default(),
            active: 0,
            queued: 0,
            persistence_errors: 0,
            last_prune: Instant::now(),
        }
    }
}

pub struct Telemetry {
    state: Arc<Mutex<State>>,
    store: PathBuf,
    retention_days: u64,
    max_events: usize,
    persist_lock: Arc<Mutex<()>>,
    dirty: Arc<Notify>,
    worker_started: AtomicBool,
    worker_cancel: CancellationToken,
    started_at: String,
    rejections: [AtomicU64; 6],
}

impl Telemetry {
    pub fn open(dir: &Path, retention_days: u64, max_events: usize) -> Result<Self, String> {
        if !(1..=365).contains(&retention_days) || !(1..=50_000).contains(&max_events) {
            return Err("invalid telemetry retention limits".into());
        }
        validate_path(dir)?;
        std::fs::create_dir_all(dir).map_err(|_| "cannot create telemetry directory")?;
        validate_path(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| "cannot protect telemetry directory")?;
        let store = dir.join("telemetry.json");
        let mut state = State::default();
        match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&store)
        {
            Ok(mut file) => {
                if !file
                    .metadata()
                    .map_err(|_| "cannot inspect telemetry")?
                    .is_file()
                {
                    return Err("telemetry store must be a regular file".into());
                }
                let mut data = Vec::new();
                std::io::Read::by_ref(&mut file)
                    .take(MAX_STORE_BYTES + 1)
                    .read_to_end(&mut data)
                    .map_err(|_| "cannot read telemetry")?;
                if data.len() as u64 > MAX_STORE_BYTES {
                    return Err("telemetry store exceeds limit".into());
                }
                // A corrupt telemetry snapshot is recoverable and must not
                // turn metadata into a dependency of generation availability.
                match serde_json::from_slice::<Vec<RequestRecord>>(&data) {
                    Ok(records) => {
                        state.records = records.into_iter().filter_map(sanitise_record).collect()
                    }
                    Err(_) => state.persistence_errors = 1,
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("cannot safely open telemetry store".into()),
        }
        prune(&mut state.records, retention_days, max_events);
        Ok(Self {
            state: Arc::new(Mutex::new(state)),
            store,
            retention_days,
            max_events,
            persist_lock: Arc::new(Mutex::new(())),
            dirty: Arc::new(Notify::new()),
            worker_started: AtomicBool::new(false),
            worker_cancel: CancellationToken::new(),
            started_at: Utc::now().to_rfc3339(),
            rejections: std::array::from_fn(|_| AtomicU64::new(0)),
        })
    }

    /// Count every rejection without allocation, disk IO or the history mutex.
    /// Arbitrary error strings map to one fixed bucket, preventing cardinality
    /// growth during an unauthenticated request flood.
    pub fn note_rejection(&self, code: &str) {
        let bucket = match code {
            "invalid_api_key" | "authentication_error" | "unauthorized" | "http_401" => 0,
            "rate_limit_exceeded" | "http_429" => 1,
            "insufficient_scope" | "http_403" => 2,
            "invalid_request"
            | "unsupported_parameter"
            | "request_too_large"
            | "input_too_large"
            | "headers_too_large"
            | "body_timeout"
            | "http_400"
            | "http_408"
            | "http_413"
            | "http_431"
            | "invalid_request_error" => 3,
            "busy" | "run_limit_exceeded" | "subscriber_limit" | "shutting_down" => 4,
            _ => 5,
        };
        let _ = self.rejections[bucket].fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
            Some(n.saturating_add(1))
        });
    }

    fn rejection_counts(&self) -> CountsSinceStart {
        let [auth, rate, scope, input, busy, other] = self
            .rejections
            .each_ref()
            .map(|counter| counter.load(Ordering::Relaxed));
        CountsSinceStart {
            started_at: self.started_at.clone(),
            reset_on_restart: true,
            total: [auth, rate, scope, input, busy, other]
                .into_iter()
                .fold(0u64, u64::saturating_add),
            auth,
            rate,
            scope,
            input,
            busy,
            other,
        }
    }

    pub async fn record(&self, record: RequestRecord) {
        let Some(record) = sanitise_record(record) else {
            return;
        };
        // Ignore an already expired insertion in constant time. Existing
        // records are scanned no more than once per minute, not per request.
        if DateTime::parse_from_rfc3339(&record.started_at)
            .is_ok_and(|at| at < Utc::now() - chrono::Duration::days(self.retention_days as i64))
        {
            return;
        }
        let mut state = self.state.lock().await;
        prune_if_due(&mut state, self.retention_days, self.max_events);
        state.records.push_back(record);
        if state.records.len() > self.max_events {
            state.records.pop_front();
        }
        drop(state);
        self.ensure_worker();
        self.dirty.notify_one();
    }

    fn ensure_worker(&self) {
        if !self.worker_started.swap(true, Ordering::SeqCst) {
            let state = self.state.clone();
            let store = self.store.clone();
            let persist_lock = self.persist_lock.clone();
            let dirty = self.dirty.clone();
            let cancel = self.worker_cancel.clone();
            let retention_days = self.retention_days;
            let max_events = self.max_events;
            tokio::spawn(async move {
                let mut expiry = tokio::time::interval_at(
                    tokio::time::Instant::now() + PRUNE_INTERVAL,
                    PRUNE_INTERVAL,
                );
                expiry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    let dirty_batch = tokio::select! {
                        _ = cancel.cancelled() => break,
                        _ = dirty.notified() => true,
                        _ = expiry.tick() => false,
                    };
                    // Batch metadata writes. Rejected HTTP requests never wait
                    // for disk and cannot induce one full-store fsync each.
                    if dirty_batch {
                        tokio::select! {
                            _ = cancel.cancelled() => break,
                            _ = tokio::time::sleep(Duration::from_secs(1)) => {},
                        }
                    }
                    flush_state(&state, &persist_lock, &store, retention_days, max_events).await;
                }
            });
        }
    }

    /// Flush during graceful shutdown; forced termination can lose the latest
    /// unflushed metadata, without affecting requests or provider state.
    pub async fn flush(&self) {
        flush_state(
            &self.state,
            &self.persist_lock,
            &self.store,
            self.retention_days,
            self.max_events,
        )
        .await;
    }

    pub async fn snapshot(&self) -> Snapshot {
        self.ensure_worker();
        let mut state = self.state.lock().await;
        prune_if_due(&mut state, self.retention_days, self.max_events);
        let records: Vec<_> = state.records.iter().cloned().collect();
        let active = state.active;
        let queued = state.queued;
        let provider = state.provider.clone();
        let persistence_errors = state.persistence_errors;
        drop(state);
        Snapshot {
            generated_at: Utc::now().to_rfc3339(),
            active,
            queued,
            provider_age_seconds: DateTime::parse_from_rfc3339(&provider.checked_at).ok().map(
                |checked| {
                    (Utc::now()
                        .signed_duration_since(checked)
                        .num_seconds()
                        .max(0)) as u64
                },
            ),
            provider,
            aggregate: aggregate(&records),
            counts_since_start: self.rejection_counts(),
            records,
            retention_days: self.retention_days,
            max_events: self.max_events,
            persistence_errors,
        }
    }

    pub async fn set_provider(&self, mut provider: ProviderStatus) {
        self.ensure_worker();
        provider.version = identifier(&provider.version, 32);
        provider.models.truncate(500);
        for model in &mut provider.models {
            model.id = identifier(&model.id, 128);
            model.name = identifier(&model.name, 128);
        }
        provider.error = provider.error.map(|error| identifier(&error, 80));
        if DateTime::parse_from_rfc3339(&provider.checked_at).is_err() {
            provider.checked_at = String::new();
        }
        provider.quota = provider.quota.and_then(sanitise_quota);
        self.state.lock().await.provider = provider;
    }

    pub async fn set_runtime(&self, active: usize, queued: usize) {
        let mut state = self.state.lock().await;
        state.active = active;
        state.queued = queued;
    }
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        self.worker_cancel.cancel();
    }
}

async fn flush_state(
    state: &Arc<Mutex<State>>,
    persist_lock: &Arc<Mutex<()>>,
    store: &Path,
    retention_days: u64,
    max_events: usize,
) {
    // Acquire before taking the snapshot so older data cannot replace newer
    // data when a background flush overlaps graceful shutdown.
    let _serial = persist_lock.lock().await;
    let records: Vec<_> = {
        let mut state = state.lock().await;
        prune_if_due(&mut state, retention_days, max_events);
        state.records.iter().cloned().collect()
    };
    let store = store.to_path_buf();
    let persisted = tokio::task::spawn_blocking(move || persist(&store, &records)).await;
    if !matches!(persisted, Ok(Ok(()))) {
        let mut state = state.lock().await;
        state.persistence_errors = state.persistence_errors.saturating_add(1);
    }
}

fn validate_path(path: &Path) -> Result<(), String> {
    if !path.is_absolute() {
        return Err("telemetry path must be absolute".into());
    }
    let mut checked = PathBuf::new();
    for component in path.components() {
        if let Component::Normal(name) = component {
            let name = name.to_string_lossy();
            if name.starts_with(".env")
                || name.ends_with(".pem")
                || name.ends_with(".key")
                || name == "secrets"
                || name == "credentials"
            {
                return Err("protected path is not telemetry storage".into());
            }
        } else if matches!(component, Component::ParentDir | Component::CurDir) {
            return Err("telemetry path must be canonical".into());
        }
        checked.push(component.as_os_str());
        match std::fs::symlink_metadata(&checked) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err("telemetry paths cannot contain symlinks".into());
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("cannot inspect telemetry path".into()),
        }
    }
    Ok(())
}

fn identifier(value: &str, max: usize) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || "._:/ -".contains(*c))
        .take(max)
        .collect()
}

fn sanitise_record(mut record: RequestRecord) -> Option<RequestRecord> {
    let started = DateTime::parse_from_rfc3339(&record.started_at).ok()?;
    if started > Utc::now() + chrono::Duration::minutes(5) {
        return None;
    }
    record.started_at = started.to_rfc3339();
    record.request_id = identifier(&record.request_id, 80);
    record.client_id = identifier(&record.client_id, 64);
    record.model = identifier(&record.model, 128);
    record.endpoint = identifier(&record.endpoint, 80);
    record.status = identifier(&record.status, 32);
    record.error_code = record.error_code.map(|value| identifier(&value, 80));
    if record.request_id.is_empty() || record.status.is_empty() {
        return None;
    }
    Some(record)
}

fn sanitise_quota(value: serde_json::Value) -> Option<serde_json::Value> {
    use serde_json::{Value, json};
    let groups = value.get("groups")?.as_array()?;
    let groups: Vec<_> = groups.iter().take(20).map(|group| {
        let buckets: Vec<_> = group.get("buckets").and_then(Value::as_array).into_iter().flatten().take(100).map(|bucket| {
            let mut result = serde_json::Map::new();
            for field in ["id", "name", "window"] {
                if let Some(value) = bucket.get(field).and_then(Value::as_str) {
                    result.insert(field.into(), json!(identifier(value, 128)));
                }
            }
            if let Some(value) = bucket.get("remaining_fraction").and_then(Value::as_f64).filter(|n| (0.0..=1.0).contains(n)) {
                result.insert("remaining_fraction".into(), json!(value));
            }
            if let Some(value) = bucket.get("reset_time").and_then(Value::as_str)
                .and_then(|value| DateTime::parse_from_rfc3339(value).ok()) {
                result.insert("reset_time".into(), json!(value.to_rfc3339()));
            }
            Value::Object(result)
        }).collect();
        json!({"name": identifier(group.get("name").and_then(Value::as_str).unwrap_or(""), 128), "buckets": buckets})
    }).collect();
    Some(json!({"groups": groups}))
}

fn prune(records: &mut VecDeque<RequestRecord>, days: u64, max: usize) {
    let cutoff = Utc::now() - chrono::Duration::days(days as i64);
    records.retain(|record| {
        DateTime::parse_from_rfc3339(&record.started_at).is_ok_and(|at| at >= cutoff)
    });
    while records.len() > max {
        records.pop_front();
    }
}

fn prune_if_due(state: &mut State, days: u64, max: usize) {
    if state.last_prune.elapsed() >= PRUNE_INTERVAL {
        prune(&mut state.records, days, max);
        state.last_prune = Instant::now();
    }
}

fn persist(path: &Path, records: &[RequestRecord]) -> Result<(), ()> {
    let tmp = path.with_file_name(format!("telemetry-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let data = serde_json::to_vec(records).map_err(|_| ())?;
        if data.len() as u64 > MAX_STORE_BYTES {
            return Err(());
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)
            .map_err(|_| ())?;
        file.write_all(&data).map_err(|_| ())?;
        file.sync_all().map_err(|_| ())?;
        std::fs::rename(&tmp, path).map_err(|_| ())?;
        Ok(())
    })();
    let _ = std::fs::remove_file(tmp);
    result
}

fn aggregate(records: &[RequestRecord]) -> Aggregate {
    let mut result = Aggregate {
        requests: records.len(),
        ..Aggregate::default()
    };
    let mut durations = Vec::new();
    let mut first_output = Vec::new();
    let mut startup = Vec::new();
    for record in records {
        *result
            .by_client
            .entry(record.client_id.clone())
            .or_default() += 1;
        *result.by_model.entry(record.model.clone()).or_default() += 1;
        durations.push(record.duration_ms);
        if let Some(first) = record.first_output_ms {
            first_output.push(first);
        }
        if let Some(ready) = record.startup_ms {
            startup.push(ready);
        }
        if let Some(error) = &record.error_code {
            result.failures += 1;
            *result.by_error.entry(error.clone()).or_default() += 1;
            if matches!(
                error.as_str(),
                "invalid_api_key" | "authentication_error" | "unauthorized"
            ) {
                result.auth_failures += 1;
            }
        }
        if let Some(usage) = &record.usage {
            if record.usage_partial {
                result.partial_usage_records += 1
            } else {
                result.complete_usage_records += 1
            }
            result.input_tokens = result.input_tokens.saturating_add(usage.input_tokens);
            result.output_tokens = result.output_tokens.saturating_add(usage.output_tokens);
            result.thinking_tokens = result.thinking_tokens.saturating_add(usage.thinking_tokens);
            result.cache_read_tokens = result
                .cache_read_tokens
                .saturating_add(usage.cache_read_tokens);
            result.total_tokens = result.total_tokens.saturating_add(usage.total_tokens);
        }
    }
    durations.sort_unstable();
    first_output.sort_unstable();
    startup.sort_unstable();
    result.duration_p50_ms = percentile(&durations, 50);
    result.duration_p95_ms = percentile(&durations, 95);
    result.first_output_p50_ms = percentile(&first_output, 50);
    result.first_output_p95_ms = percentile(&first_output, 95);
    result.startup_p50_ms = percentile(&startup, 50);
    result.startup_p95_ms = percentile(&startup, 95);
    result
}

fn percentile(sorted: &[u64], percent: usize) -> Option<u64> {
    if sorted.is_empty() {
        None
    } else {
        Some(sorted[(sorted.len() * percent).div_ceil(100).saturating_sub(1)])
    }
}

struct SocketGuard {
    path: PathBuf,
    inode: u64,
    device: u64,
}
impl Drop for SocketGuard {
    fn drop(&mut self) {
        if let Ok(metadata) = std::fs::symlink_metadata(&self.path) {
            if metadata.ino() == self.inode && metadata.dev() == self.device {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}

pub async fn serve(
    telemetry: Arc<Telemetry>,
    socket: PathBuf,
    cancel: CancellationToken,
) -> Result<(), String> {
    validate_path(&socket)?;
    let parent = socket.parent().ok_or("monitor socket needs a parent")?;
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(|_| "cannot create monitor socket directory")?;
    validate_path(&socket)?;
    if let Ok(metadata) = std::fs::symlink_metadata(&socket) {
        use std::os::unix::fs::FileTypeExt;
        if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() } {
            return Err("refusing to replace a non-owned monitor socket path".into());
        }
        if UnixStream::connect(&socket).await.is_ok() {
            return Err("monitor socket already active".into());
        }
        tokio::fs::remove_file(&socket)
            .await
            .map_err(|_| "cannot remove stale monitor socket")?;
    }
    let listener = UnixListener::bind(&socket).map_err(|_| "cannot bind monitor socket")?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
        .map_err(|_| "cannot protect monitor socket")?;
    let metadata =
        std::fs::symlink_metadata(&socket).map_err(|_| "cannot inspect monitor socket")?;
    let _guard = SocketGuard {
        path: socket,
        inode: metadata.ino(),
        device: metadata.dev(),
    };
    let connections = Arc::new(Semaphore::new(8));
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            accepted = listener.accept() => {
                let (mut stream, _) = accepted.map_err(|_| "monitor accept failed")?;
                let Ok(permit) = connections.clone().try_acquire_owned() else { continue };
                let telemetry = telemetry.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    let _ = tokio::time::timeout(Duration::from_secs(5), async move {
                        let mut command = Vec::new();
                        loop {
                            let byte = stream.read_u8().await?;
                            if byte == b'\n' { break }
                            if command.len() >= 16 { return Ok::<_, std::io::Error>(()) }
                            command.push(byte);
                        }
                        if command == b"snapshot" {
                            let mut json = serde_json::to_vec(&telemetry.snapshot().await).map_err(std::io::Error::other)?;
                            json.push(b'\n');
                            stream.write_all(&json).await?;
                        }
                        stream.shutdown().await
                    }).await;
                });
            }
        }
    }
}

pub struct MonitorArgs {
    pub json: bool,
    pub text: bool,
    pub range_seconds: u64,
}

async fn get_snapshot(socket: &Path, range_seconds: u64) -> Result<Snapshot, String> {
    validate_path(socket)?;
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut stream = UnixStream::connect(socket)
            .await
            .map_err(|_| "monitor is unavailable")?;
        stream
            .write_all(b"snapshot\n")
            .await
            .map_err(|_| "monitor write failed")?;
        let mut data = Vec::new();
        stream
            .take(MAX_STORE_BYTES + 1)
            .read_to_end(&mut data)
            .await
            .map_err(|_| "monitor read failed")?;
        if data.len() as u64 > MAX_STORE_BYTES {
            return Err("monitor snapshot exceeds limit");
        }
        let mut snapshot: Snapshot =
            serde_json::from_slice(&data).map_err(|_| "invalid monitor snapshot")?;
        if range_seconds != 0 {
            let cutoff =
                Utc::now() - chrono::Duration::seconds(range_seconds.min(365 * 86400) as i64);
            snapshot.records.retain(|record| {
                DateTime::parse_from_rfc3339(&record.started_at).is_ok_and(|at| at >= cutoff)
            });
            snapshot.aggregate = aggregate(&snapshot.records);
        }
        Ok(snapshot)
    })
    .await
    .map_err(|_| "monitor timed out")?
    .map_err(str::to_string)
}

fn render(snapshot: &Snapshot) -> String {
    let a = &snapshot.aggregate;
    let c = &snapshot.counts_since_start;
    let mut text = format!(
        "AI router  {}\nAGY {}  authenticated={}  checked={}  age={}s\nActive={}  queued={}  requests={}  failures={}  auth failures={}\nLatency p50/p95={} / {} ms  AGY ready={} / {} ms  first output={} / {} ms\nObserved tokens input={} output={} total={} (complete={} partial={})\nRetention={} days / {} events  persistence errors={}\n",
        snapshot.generated_at,
        snapshot.provider.version,
        snapshot.provider.authenticated,
        snapshot.provider.checked_at,
        fmt_metric(snapshot.provider_age_seconds),
        snapshot.active,
        snapshot.queued,
        a.requests,
        a.failures,
        a.auth_failures,
        fmt_metric(a.duration_p50_ms),
        fmt_metric(a.duration_p95_ms),
        fmt_metric(a.startup_p50_ms),
        fmt_metric(a.startup_p95_ms),
        fmt_metric(a.first_output_p50_ms),
        fmt_metric(a.first_output_p95_ms),
        a.input_tokens,
        a.output_tokens,
        a.total_tokens,
        a.complete_usage_records,
        a.partial_usage_records,
        snapshot.retention_days,
        snapshot.max_events,
        snapshot.persistence_errors,
    );
    text.push_str(&format!(
        "Rejected since {}: total={} auth={} rate={} scope={} input={} busy={} other={} (reset on restart; recent rejection history is sampled)\n",
        c.started_at, c.total, c.auth, c.rate, c.scope, c.input, c.busy, c.other,
    ));
    if let Some(error) = &snapshot.provider.error {
        text.push_str(&format!("Provider status: {error}\n"));
    }
    if let Some(quota) = &snapshot.provider.quota {
        text.push_str(&format!("Quota: {quota}\n"));
    }
    text.push_str("\nRecent requests (newest first)\n");
    for record in snapshot.records.iter().rev().take(15) {
        text.push_str(&format!(
            "{}  {}  {}  {}ms  {}  {}\n",
            record.request_id,
            record.client_id,
            record.model,
            record.duration_ms,
            record.status,
            record.error_code.as_deref().unwrap_or("")
        ));
    }
    text
}
fn fmt_metric(value: Option<u64>) -> String {
    value
        .map(|n| n.to_string())
        .unwrap_or_else(|| "unknown".into())
}

pub async fn client(socket: &Path, args: MonitorArgs) -> Result<(), String> {
    let interactive = !args.json && !args.text && std::io::stdout().is_terminal();
    loop {
        let snapshot = get_snapshot(socket, args.range_seconds).await?;
        if args.json {
            println!(
                "{}",
                serde_json::to_string(&snapshot).map_err(|_| "cannot encode snapshot")?
            );
        } else {
            if interactive {
                print!("\x1b[2J\x1b[H");
            }
            print!("{}", render(&snapshot));
            std::io::stdout()
                .flush()
                .map_err(|_| "cannot write monitor output")?;
        }
        if !interactive {
            return Ok(());
        }
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Ok(()),
            _ = tokio::time::sleep(Duration::from_secs(2)) => {},
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record(id: &str) -> RequestRecord {
        RequestRecord {
            request_id: id.into(),
            client_id: "test".into(),
            model: "model".into(),
            endpoint: "/v1/chat/completions".into(),
            started_at: Utc::now().to_rfc3339(),
            duration_ms: 100,
            startup_ms: Some(10),
            first_output_ms: Some(20),
            status: "completed".into(),
            error_code: None,
            usage: None,
            usage_partial: false,
        }
    }
    #[tokio::test]
    async fn persists_bounded_records_and_expires_old_entries() {
        let dir = tempfile::tempdir().unwrap();
        let telemetry = Telemetry::open(dir.path(), 30, 2).unwrap();
        let mut old = record("old");
        old.started_at = (Utc::now() - chrono::Duration::days(31)).to_rfc3339();
        telemetry.record(old).await;
        for id in ["one", "two", "three"] {
            telemetry.record(record(id)).await;
        }
        let snapshot = telemetry.snapshot().await;
        assert_eq!(
            snapshot
                .records
                .iter()
                .map(|r| r.request_id.as_str())
                .collect::<Vec<_>>(),
            ["two", "three"]
        );
        telemetry.flush().await;
        drop(telemetry);
        let recovered = Telemetry::open(dir.path(), 30, 2).unwrap().snapshot().await;
        assert_eq!(recovered.aggregate.requests, 2);
        assert_eq!(
            std::fs::metadata(dir.path().join("telemetry.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    #[tokio::test]
    async fn rejection_flood_keeps_full_history_and_fixed_counter_cardinality() {
        let dir = tempfile::tempdir().unwrap();
        let records: Vec<_> = (0..50_000)
            .map(|n| record(&format!("request-{n}")))
            .collect();
        persist(&dir.path().join("telemetry.json"), &records).unwrap();
        let telemetry = Telemetry::open(dir.path(), 30, 50_000).unwrap();
        for n in 0..10_000 {
            match n % 6 {
                0 => telemetry.note_rejection("invalid_api_key"),
                1 => telemetry.note_rejection("rate_limit_exceeded"),
                2 => telemetry.note_rejection("insufficient_scope"),
                3 => telemetry.note_rejection("invalid_request"),
                4 => telemetry.note_rejection("busy"),
                _ => telemetry.note_rejection(&format!("attacker-code-{n}")),
            }
        }
        let snapshot = telemetry.snapshot().await;
        assert_eq!(snapshot.records.len(), 50_000);
        assert_eq!(snapshot.records.first().unwrap().request_id, "request-0");
        assert_eq!(snapshot.records.last().unwrap().request_id, "request-49999");
        let counts = &snapshot.counts_since_start;
        assert_eq!(counts.total, 10_000);
        assert_eq!(
            [
                counts.auth,
                counts.rate,
                counts.scope,
                counts.input,
                counts.busy,
                counts.other
            ],
            [1667, 1667, 1667, 1667, 1666, 1666]
        );
        assert!(counts.reset_on_restart);
        assert!(DateTime::parse_from_rfc3339(&counts.started_at).is_ok());
        assert_eq!(
            serde_json::to_value(counts)
                .unwrap()
                .as_object()
                .unwrap()
                .len(),
            9
        );
        // A genuine recorded event uses constant-time oldest-entry eviction.
        telemetry.record(record("new-request")).await;
        let snapshot = telemetry.snapshot().await;
        assert_eq!(snapshot.records.len(), 50_000);
        assert_eq!(snapshot.records.first().unwrap().request_id, "request-1");
        assert_eq!(snapshot.records.last().unwrap().request_id, "new-request");
        telemetry.flush().await;
        drop(telemetry);
        let recovered = Telemetry::open(dir.path(), 30, 50_000).unwrap();
        let snapshot = recovered.snapshot().await;
        assert_eq!(snapshot.records.len(), 50_000);
        assert_eq!(snapshot.records.first().unwrap().request_id, "request-1");
        assert_eq!(snapshot.records.last().unwrap().request_id, "new-request");
        assert_eq!(snapshot.counts_since_start.total, 0);
    }
    #[tokio::test]
    async fn expiration_scan_runs_only_when_periodic_prune_is_due() {
        let dir = tempfile::tempdir().unwrap();
        let telemetry = Telemetry::open(dir.path(), 30, 20).unwrap();
        telemetry.record(record("live")).await;
        let last_prune = {
            let mut state = telemetry.state.lock().await;
            let mut expired = record("expired-between-scans");
            expired.started_at = (Utc::now() - chrono::Duration::days(31)).to_rfc3339();
            // Model an entry aging out after the last scan, without a flaky
            // real-time delay. Completion order need not be timestamp order.
            state.records.push_back(expired);
            state.last_prune
        };
        telemetry.record(record("newer")).await;
        assert_eq!(telemetry.snapshot().await.records.len(), 3);
        assert_eq!(telemetry.state.lock().await.last_prune, last_prune);
        telemetry.state.lock().await.last_prune = Instant::now() - PRUNE_INTERVAL;
        assert_eq!(telemetry.snapshot().await.records.len(), 2);
        assert!(telemetry.state.lock().await.last_prune > last_prune);
        telemetry.flush().await;
        assert_eq!(
            Telemetry::open(dir.path(), 30, 20)
                .unwrap()
                .snapshot()
                .await
                .records
                .len(),
            2
        );
    }
    #[tokio::test]
    async fn socket_is_private_read_only_and_removed_on_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let telemetry = Arc::new(Telemetry::open(&dir.path().join("data"), 30, 20).unwrap());
        telemetry.record(record("test")).await;
        let socket = dir.path().join("monitor.sock");
        let cancel = CancellationToken::new();
        let server = tokio::spawn(serve(telemetry, socket.clone(), cancel.clone()));
        for _ in 0..50 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            get_snapshot(&socket, 3600)
                .await
                .unwrap()
                .aggregate
                .requests,
            1
        );
        let mut stream = UnixStream::connect(&socket).await.unwrap();
        stream.write_all(b"delete\n").await.unwrap();
        let mut data = Vec::new();
        stream.read_to_end(&mut data).await.unwrap();
        assert!(data.is_empty());
        cancel.cancel();
        server.await.unwrap().unwrap();
        assert!(!socket.exists());
    }
    #[test]
    fn refuses_protected_paths_and_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        for path in [
            ".env-dummy",
            "dummy.pem",
            "dummy.key",
            "secrets/dummy",
            "credentials/dummy",
        ] {
            assert!(Telemetry::open(&dir.path().join(path), 30, 2).is_err());
        }
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(target, &link).unwrap();
        assert!(Telemetry::open(&link, 30, 2).is_err());
    }
    #[tokio::test]
    async fn refuses_to_replace_an_existing_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let telemetry = Arc::new(Telemetry::open(&dir.path().join("data"), 30, 20).unwrap());
        let socket = dir.path().join("monitor.sock");
        std::fs::write(&socket, b"synthetic canary").unwrap();
        assert!(
            serve(telemetry, socket.clone(), CancellationToken::new())
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&socket).unwrap(), b"synthetic canary");
    }
    #[tokio::test]
    async fn persistence_failure_preserves_in_memory_monitoring() {
        let dir = tempfile::tempdir().unwrap();
        let telemetry = Telemetry::open(dir.path(), 30, 20).unwrap();
        // Synthetic obstruction: rename to an existing directory must fail.
        std::fs::create_dir(dir.path().join("telemetry.json")).unwrap();
        telemetry.record(record("still-visible")).await;
        telemetry.flush().await;
        let snapshot = telemetry.snapshot().await;
        assert_eq!(snapshot.aggregate.requests, 1);
        assert_eq!(snapshot.persistence_errors, 1);
    }
    #[test]
    fn quota_and_identifiers_do_not_preserve_payload_fields_or_control_sequences() {
        let quota = sanitise_quota(serde_json::json!({"groups": [{"name": "Gemini", "description": "prompt",
            "buckets": [{"id": "one", "remaining_fraction": 0.5, "token": "credential", "response": "text"}]}], "secret": "secret"})).unwrap();
        assert_eq!(
            quota,
            serde_json::json!({"groups":[{"name":"Gemini","buckets":[{"id":"one","remaining_fraction":0.5}]}]})
        );
        assert!(!identifier("\u{1b}[31mtest\n", 80).contains('\u{1b}'));
    }
}
