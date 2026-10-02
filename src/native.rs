use crate::{
    api::{AppState, CompletionGuard, error},
    config::KeyConfig,
    protocol::*,
};
use axum::{
    Extension, Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::Utc;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    convert::Infallible,
    fs::{File, OpenOptions},
    io::Read,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Component, Path as FsPath, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, Notify, Semaphore};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Default)]
pub struct RunStore {
    jobs: Mutex<HashMap<String, Arc<Job>>>,
}
struct Job {
    id: String,
    owner: String,
    model: String,
    created: Instant,
    created_at: String,
    workspace: PathBuf,
    workspace_owned: AtomicBool,
    cancel: CancellationToken,
    changed: Notify,
    data: Mutex<JobData>,
    subscribers: Arc<Semaphore>,
    finished: CancellationToken,
}
struct JobData {
    status: String,
    result: Option<RunResult>,
    error: Option<ApiError>,
    events: VecDeque<(u64, Value)>,
    bytes: usize,
    next: u64,
}

impl Job {
    async fn push(&self, event: Value, limit: usize) -> bool {
        let mut data = self.data.lock().await;
        let bytes = event.to_string().len();
        if data.bytes.saturating_add(bytes) > limit || data.events.len() >= 4096 {
            return false;
        }
        let seq = data.next;
        data.next += 1;
        data.bytes += bytes;
        data.events.push_back((seq, event));
        drop(data);
        self.changed.notify_waiters();
        true
    }
    async fn view(&self) -> Value {
        let data = self.data.lock().await;
        json!({"id":self.id,"object":"agy.run","model":self.model,"created_at":self.created_at,"status":data.status,"result":data.result,"error":data.error.as_ref().map(|e|json!({"code":e.code,"message":e.message})),"events_url":format!("/v1/agy/runs/{}/events",self.id),"artifacts_url":format!("/v1/agy/runs/{}/artifacts",self.id)})
    }
}

pub async fn create(
    State(state): State<Arc<AppState>>,
    Extension(key): Extension<KeyConfig>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, ApiError> {
    if !state.config.native_enabled {
        return Err(error(
            503,
            "native_profile_disabled",
            "Native runs require operator verification of AGY sandbox containment",
        ));
    }
    let Json(body) = body.map_err(|e| {
        error(
            e.status().as_u16(),
            "invalid_request",
            "Expected a bounded JSON request body",
        )
    })?;
    let map = body
        .as_object()
        .ok_or_else(|| error(400, "invalid_request", "Expected a JSON object"))?;
    let allowed = ["model", "prompt", "effort", "mode", "json_schema", "files"];
    if map.keys().any(|k| !allowed.contains(&k.as_str())) {
        return Err(error(
            400,
            "unsupported_parameter",
            "Unsupported native run parameter",
        ));
    }
    let model = map
        .get("model")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| error(400, "invalid_request", "model is required"))?
        .to_owned();
    let prompt = map
        .get("prompt")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| error(400, "invalid_request", "prompt is required"))?
        .to_owned();
    let effort = match map.get("effort") {
        None | Some(Value::Null) => None,
        Some(Value::String(s))
            if ["low", "medium", "high", "xhigh", "max"].contains(&s.as_str()) =>
        {
            Some(s.clone())
        }
        _ => return Err(error(400, "invalid_request", "Invalid effort")),
    };
    let mode = match map.get("mode") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if ["plan", "accept-edits"].contains(&s.as_str()) => Some(s.clone()),
        _ => return Err(error(400, "invalid_request", "Invalid mode")),
    };
    let schema = match map.get("json_schema") {
        None | Some(Value::Null) => None,
        Some(v) if v.is_object() => Some(v.clone()),
        _ => {
            return Err(error(
                400,
                "invalid_schema",
                "json_schema must be an object",
            ));
        }
    };
    if let Some(schema) = &schema {
        validate_schema(schema)?;
    }
    let files = decode_files(map.get("files"), state.config.max_body_bytes)?;
    state.require_model(&model).await?;
    let admission = state.admit().await?;
    let id = Uuid::new_v4().to_string();
    let workspace = state.config.workspace_dir.join(&id);
    let job = Arc::new(Job {
        id: id.clone(),
        owner: key.id.clone(),
        model: model.clone(),
        created: Instant::now(),
        created_at: Utc::now().to_rfc3339(),
        workspace: workspace.clone(),
        workspace_owned: AtomicBool::new(false),
        cancel: state.shutdown.child_token(),
        changed: Notify::new(),
        subscribers: Arc::new(Semaphore::new(2)),
        finished: CancellationToken::new(),
        data: Mutex::new(JobData {
            status: "queued".into(),
            result: None,
            error: None,
            events: VecDeque::new(),
            bytes: 0,
            next: 0,
        }),
    });
    {
        let mut jobs = state.runs.jobs.lock().await;
        if jobs.len() >= 16 {
            return Err(error(
                429,
                "run_limit_exceeded",
                "Too many retained runs; wait for expiry or cancel existing runs",
            ));
        }
        jobs.insert(id.clone(), job.clone());
    }
    let request = RunRequest {
        request_id: id,
        model,
        prompt,
        system: String::new(),
        tools: Vec::new(),
        schema,
        profile: RunProfile::Native,
        effort,
        mode,
        files,
    };
    let task_state = state.clone();
    let task_job = job.clone();
    tokio::spawn(async move {
        execute(task_state, task_job, key, request, admission).await;
    });
    Ok((StatusCode::ACCEPTED, Json(job.view().await)).into_response())
}

async fn execute(
    state: Arc<AppState>,
    job: Arc<Job>,
    key: KeyConfig,
    request: RunRequest,
    admission: crate::api::Admission,
) {
    let mut provider_finished = None;
    let mut provider_owned = None;
    let outcome: Result<(), ApiError> = async {
        let started = Instant::now();
        if job.cancel.is_cancelled() {
            return Err(error(499, "cancelled", "Run cancelled"));
        }
        job.data.lock().await.status = "running".into();
        let mut run = state.runner.start(request, job.cancel.clone()).await?;
        provider_finished = Some(run.finished.clone());
        provider_owned = Some(run.workspace_owned.clone());
        let mut guard = CompletionGuard::new(
            state.clone(),
            key.id,
            job.model.clone(),
            "native",
            job.id.clone(),
            started,
            run.workspace.clone(),
            run.workspace_owned.clone(),
            job.cancel.clone(),
            run.finished.clone(),
            admission,
        );
        guard.preserve_workspace();
        let mut terminal = false;
        while let Some(event) = run.events.recv().await {
            guard.observe(&event);
            if let RunEvent::Error(e) = event {
                return Err(e);
            }
            let value = serde_json::to_value(&event)
                .map_err(|_| error(500, "event_encoding", "Could not encode native event"))?;
            if !job
                .push(value, state.config.max_output_bytes.min(2 * 1024 * 1024))
                .await
            {
                let failure = error(
                    502,
                    "output_limit_exceeded",
                    "Native output exceeded the configured bound",
                );
                guard.observe(&RunEvent::Error(failure.clone()));
                return Err(failure);
            }
            match event {
                RunEvent::Completed(result) => {
                    run.finished.cancelled().await;
                    let mut data = job.data.lock().await;
                    data.status = "completed".into();
                    data.result = Some(result);
                    terminal = true;
                    break;
                }
                RunEvent::Error(e) => return Err(e),
                _ => {}
            }
        }
        if !terminal {
            let failure = error(
                502,
                "incomplete_provider_output",
                "AGY ended without a result",
            );
            guard.observe(&RunEvent::Error(failure.clone()));
            return Err(failure);
        }
        Ok(())
    }
    .await;
    if outcome.is_err() {
        job.cancel.cancel();
    }
    if let Some(finished) = provider_finished {
        if tokio::time::timeout(Duration::from_secs(30), finished.cancelled())
            .await
            .is_err()
        {
            return;
        }
    }
    if let Some(owned) = provider_owned {
        job.workspace_owned
            .store(owned.load(Ordering::Acquire), Ordering::Release);
    }
    if let Err(e) = outcome {
        let event =
            json!({"type":"error","data":{"status":e.status,"code":e.code,"message":e.message}});
        let mut data = job.data.lock().await;
        data.status = if e.code == "cancelled" || e.code == "request_cancelled" {
            "cancelled"
        } else {
            "failed"
        }
        .into();
        data.error = Some(e);
        while data.events.len() >= 4096
            || data.bytes.saturating_add(event.to_string().len())
                > state
                    .config
                    .max_output_bytes
                    .min(2 * 1024 * 1024)
                    .saturating_add(1024)
        {
            if let Some((_, old)) = data.events.pop_back() {
                data.bytes = data.bytes.saturating_sub(old.to_string().len());
            } else {
                break;
            }
        }
        let seq = data.next;
        data.next += 1;
        data.bytes += event.to_string().len();
        data.events.push_back((seq, event));
    }
    job.finished.cancel();
    job.changed.notify_waiters();
}

fn decode_files(value: Option<&Value>, limit: usize) -> Result<Vec<InputFile>, ApiError> {
    let Some(value) = value.filter(|v| !v.is_null()) else {
        return Ok(Vec::new());
    };
    let items = value
        .as_array()
        .ok_or_else(|| error(400, "invalid_files", "files must be an array"))?;
    if items.len() > 64 {
        return Err(error(400, "invalid_files", "Too many files"));
    }
    let mut total = 0usize;
    let mut paths = std::collections::HashSet::new();
    let mut output = Vec::new();
    for item in items {
        let obj = item
            .as_object()
            .ok_or_else(|| error(400, "invalid_files", "Each file must be an object"))?;
        if obj.len() != 2 {
            return Err(error(
                400,
                "invalid_files",
                "Files require exactly path and data_base64",
            ));
        }
        let path = obj
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| error(400, "invalid_files", "File path is required"))?;
        safe_relative(path)?;
        if !paths.insert(path.to_owned()) {
            return Err(error(400, "invalid_files", "Duplicate file path"));
        }
        let data = STANDARD
            .decode(
                obj.get("data_base64")
                    .and_then(Value::as_str)
                    .ok_or_else(|| error(400, "invalid_files", "File data_base64 is required"))?,
            )
            .map_err(|_| error(400, "invalid_files", "Invalid base64 file data"))?;
        total = total.saturating_add(data.len());
        if total > limit {
            return Err(error(
                413,
                "request_too_large",
                "Uploaded files exceed the configured bound",
            ));
        }
        output.push(InputFile {
            path: path.to_owned(),
            data,
        });
    }
    Ok(output)
}

async fn owned(state: &AppState, key: &KeyConfig, id: &str) -> Result<Arc<Job>, ApiError> {
    let jobs = state.runs.jobs.lock().await;
    jobs.get(id)
        .filter(|job| job.owner == key.id)
        .cloned()
        .ok_or_else(|| error(404, "run_not_found", "Run not found"))
}
pub async fn status(
    State(state): State<Arc<AppState>>,
    Extension(key): Extension<KeyConfig>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(owned(&state, &key, &id).await?.view().await))
}
pub async fn cancel(
    State(state): State<Arc<AppState>>,
    Extension(key): Extension<KeyConfig>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let job = owned(&state, &key, &id).await?;
    job.cancel.cancel();
    let mut data = job.data.lock().await;
    let terminal = !["queued", "running", "cancellation_requested"].contains(&data.status.as_str());
    if !terminal {
        data.status = "cancellation_requested".into();
    }
    drop(data);
    job.changed.notify_waiters();
    if terminal {
        tokio::time::timeout(Duration::from_secs(5), job.finished.cancelled())
            .await
            .map_err(|_| error(503, "cleanup_pending", "Provider cleanup is still pending"))?;
        state.runs.jobs.lock().await.remove(&id);
        if job.workspace_owned.load(Ordering::Acquire) {
            let _ = tokio::fs::remove_dir_all(&job.workspace).await;
        }
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    Ok((StatusCode::ACCEPTED, Json(job.view().await)).into_response())
}

pub async fn events(
    State(state): State<Arc<AppState>>,
    Extension(key): Extension<KeyConfig>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let job = owned(&state, &key, &id).await?;
    let permit = job.subscribers.clone().try_acquire_owned().map_err(|_| {
        error(
            429,
            "subscriber_limit",
            "Too many event subscribers for this run",
        )
    })?;
    let mut next = match headers.get("last-event-id") {
        Some(v) => v
            .to_str()
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .and_then(|n| n.checked_add(1))
            .ok_or_else(|| error(400, "invalid_event_id", "Invalid Last-Event-ID"))?,
        None => 0,
    };
    {
        let data = job.data.lock().await;
        if next > data.next {
            return Err(error(
                400,
                "invalid_event_id",
                "Last-Event-ID is ahead of this run",
            ));
        }
    }
    let stream = async_stream::stream! {
        let _permit=permit;
        loop {
            // Register before inspecting state to avoid losing the final notification.
            let notified=job.changed.notified();tokio::pin!(notified);notified.as_mut().enable();
            let (batch,done)={let data=job.data.lock().await;(data.events.iter().filter(|(seq,_)|*seq>=next).cloned().collect::<Vec<_>>(),!["queued","running","cancellation_requested"].contains(&data.status.as_str()))};
            for (seq,value) in batch {next=seq+1;yield Ok::<Event,Infallible>(Event::default().id(seq.to_string()).event("agy.event").data(value.to_string()));}
            if done {break;}
            notified.await;
        }
    };
    let mut response = Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response();
    response
        .headers_mut()
        .insert("x-accel-buffering", "no".parse().unwrap());
    Ok(response)
}

fn safe_relative(path: &str) -> Result<(), ApiError> {
    if path.is_empty()
        || path.len() > 1024
        || path.contains('\\')
        || path.chars().any(char::is_control)
        || path.split('/').any(|part| part.is_empty() || part == ".")
    {
        return Err(error(400, "invalid_path", "Invalid workspace path"));
    }
    for component in FsPath::new(path).components() {
        let Component::Normal(part) = component else {
            return Err(error(
                400,
                "invalid_path",
                "Path must stay inside the workspace",
            ));
        };
        let name = part
            .to_str()
            .ok_or_else(|| error(400, "invalid_path", "Invalid path encoding"))?
            .to_ascii_lowercase();
        if name.starts_with('.')
            || name.ends_with(".pem")
            || name.ends_with(".key")
            || [
                "secrets",
                "credentials",
                "tmp",
                "router-tools.json",
                "capture.json",
                "router-handoff.json",
                "router-model.md",
                "router-settings.json",
            ]
            .contains(&name.as_str())
        {
            return Err(error(
                403,
                "protected_path",
                "This workspace path is protected",
            ));
        }
    }
    Ok(())
}

fn open_no_follow(root: &FsPath, relative: &str) -> Result<File, ApiError> {
    safe_relative(relative)?;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(root)
        .map_err(|_| error(404, "artifact_not_found", "Artifact not found"))?;
    let components: Vec<_> = FsPath::new(relative).components().collect();
    for (i, component) in components.iter().enumerate() {
        let Component::Normal(part) = component else {
            unreachable!()
        };
        let name = std::ffi::CString::new(part.as_encoded_bytes())
            .map_err(|_| error(400, "invalid_path", "Invalid path"))?;
        let flags = libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if i + 1 < components.len() {
                libc::O_DIRECTORY
            } else {
                0
            };
        // Each component is opened relative to the already-held directory FD;
        // a rename or symlink replacement cannot redirect us outside the root.
        let fd = unsafe { libc::openat(file.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(error(404, "artifact_not_found", "Artifact not found"));
        }
        file = unsafe { File::from_raw_fd(fd) };
    }
    let meta = file
        .metadata()
        .map_err(|_| error(404, "artifact_not_found", "Artifact not found"))?;
    if !meta.is_file() || meta.nlink() != 1 {
        return Err(error(404, "artifact_not_found", "Artifact not found"));
    }
    Ok(file)
}

pub async fn artifacts(
    State(state): State<Arc<AppState>>,
    Extension(key): Extension<KeyConfig>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let job = owned(&state, &key, &id).await?;
    if job.data.lock().await.status != "completed" {
        return Err(error(
            409,
            "run_not_complete",
            "Artifacts are available after a successful run",
        ));
    }
    let root = job.workspace.clone();
    let limit = state.config.max_output_bytes;
    let files = tokio::task::spawn_blocking(move || list_artifacts(&root, limit))
        .await
        .map_err(|_| error(500, "artifact_error", "Could not list artifacts"))??;
    Ok(Json(json!({"object":"list","data":files})))
}

fn list_artifacts(root: &FsPath, limit: usize) -> Result<Vec<Value>, ApiError> {
    let mut files = Vec::new();
    let mut pending = vec![(PathBuf::new(), 0usize)];
    let mut visited = 0usize;
    while let Some((relative, depth)) = pending.pop() {
        if depth > 16 {
            continue;
        }
        let entries = std::fs::read_dir(root.join(&relative))
            .map_err(|_| error(404, "artifact_not_found", "Workspace not found"))?;
        for entry in entries.flatten() {
            visited += 1;
            if visited > 4096 {
                return Ok(files);
            }
            let path = relative.join(entry.file_name());
            let Some(name) = path.to_str() else { continue };
            if safe_relative(name).is_err() {
                continue;
            }
            let kind = entry
                .file_type()
                .map_err(|_| error(500, "artifact_error", "Could not inspect artifact"))?;
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                if pending.len() < 256 {
                    pending.push((path, depth + 1));
                }
            } else if kind.is_file() {
                let Ok(file) = open_no_follow(root, name) else {
                    continue;
                };
                let Ok(meta) = file.metadata() else { continue };
                if meta.len() <= limit as u64 {
                    files.push(json!({"path":name,"size":meta.len()}));
                }
                if files.len() >= 256 {
                    return Ok(files);
                }
            }
        }
    }
    Ok(files)
}

pub async fn download(
    State(state): State<Arc<AppState>>,
    Extension(key): Extension<KeyConfig>,
    Path((id, path)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let job = owned(&state, &key, &id).await?;
    if job.data.lock().await.status != "completed" {
        return Err(error(
            409,
            "run_not_complete",
            "Artifacts are available after a successful run",
        ));
    }
    safe_relative(&path)?;
    let root = job.workspace.clone();
    let limit = state.config.max_output_bytes;
    let bytes = tokio::task::spawn_blocking(move || {
        let file = open_no_follow(&root, &path)?;
        if file
            .metadata()
            .map_err(|_| error(404, "artifact_not_found", "Artifact not found"))?
            .len()
            > limit as u64
        {
            return Err(error(
                413,
                "artifact_too_large",
                "Artifact exceeds download bound",
            ));
        }
        let mut data = Vec::new();
        file.take(limit as u64 + 1)
            .read_to_end(&mut data)
            .map_err(|_| error(500, "artifact_error", "Could not read artifact"))?;
        if data.len() > limit {
            return Err(error(
                413,
                "artifact_too_large",
                "Artifact exceeds download bound",
            ));
        }
        Ok(data)
    })
    .await
    .map_err(|_| error(500, "artifact_error", "Could not read artifact"))??;
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream"),
            (header::CONTENT_DISPOSITION, "attachment"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        bytes,
    )
        .into_response())
}

pub async fn reap(state: &AppState) {
    let expired = {
        let mut jobs = state.runs.jobs.lock().await;
        let mut expired = Vec::new();
        let ids: Vec<_> = jobs
            .iter()
            .filter(|(_, job)| job.created.elapsed().as_secs() > state.config.artifact_ttl_secs)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            if let Some(job) = jobs.remove(&id) {
                expired.push(job);
            }
        }
        expired
    };
    for job in expired {
        job.cancel.cancel();
        if tokio::time::timeout(Duration::from_secs(30), job.finished.cancelled())
            .await
            .is_ok()
            && job.workspace_owned.load(Ordering::Acquire)
        {
            let _ = tokio::fs::remove_dir_all(&job.workspace).await;
        }
        job.changed.notify_waiters();
    }
}

pub async fn shutdown(state: &AppState) -> bool {
    let jobs: Vec<_> = state.runs.jobs.lock().await.values().cloned().collect();
    for job in &jobs {
        job.cancel.cancel();
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut cleaned = true;
    for job in jobs {
        let result = tokio::time::timeout_at(deadline, async {
            job.finished.cancelled().await;
            if job.workspace_owned.load(Ordering::Acquire) {
                match tokio::fs::remove_dir_all(&job.workspace).await {
                    Ok(()) => job.workspace_owned.store(false, Ordering::Release),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        job.workspace_owned.store(false, Ordering::Release)
                    }
                    Err(_) => return false,
                }
            }
            true
        })
        .await;
        cleaned &= matches!(result, Ok(true));
    }
    cleaned
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn shutdown_reports_failed_owned_workspace_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let config = Arc::new(crate::config::Config::for_test(root.path()));
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        let workspace = config.workspace_dir.join("synthetic-cleanup-failure");
        // A changed file type cannot be removed as an owned directory. This
        // synthetic fault must prevent a successful shutdown report.
        std::fs::write(&workspace, b"synthetic").unwrap();
        let telemetry =
            Arc::new(crate::monitor::Telemetry::open(&config.telemetry_dir, 30, 50_000).unwrap());
        let state = AppState::new(
            config.clone(),
            crate::runner::Runner::new(config),
            telemetry,
        );
        let finished = CancellationToken::new();
        finished.cancel();
        let job = Arc::new(Job {
            id: "synthetic-cleanup-failure".into(),
            owner: "test".into(),
            model: "synthetic".into(),
            created: Instant::now(),
            created_at: Utc::now().to_rfc3339(),
            workspace: workspace.clone(),
            workspace_owned: AtomicBool::new(true),
            cancel: CancellationToken::new(),
            changed: Notify::new(),
            subscribers: Arc::new(Semaphore::new(2)),
            finished,
            data: Mutex::new(JobData {
                status: "completed".into(),
                result: None,
                error: None,
                events: VecDeque::new(),
                bytes: 0,
                next: 0,
            }),
        });
        state.runs.jobs.lock().await.insert(job.id.clone(), job);
        assert!(!shutdown(&state).await);
        assert!(std::fs::metadata(workspace).unwrap().is_file());
    }

    #[test]
    fn protected_names_and_traversal_rejected_without_reading() {
        for path in [
            "../other",
            "/absolute",
            "a/../b",
            ".env",
            "a/.env.example",
            "a/x.pem",
            "x.key",
            "secrets/x",
            "a/credentials/x",
            ".agents/a.md",
            "router-tools.json",
            "a\\b",
        ] {
            assert!(safe_relative(path).is_err(), "{path}");
        }
        assert!(safe_relative("outputs/picture.png").is_ok());
    }
    #[test]
    fn symlinks_and_special_files_never_opened() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("ok.txt"), b"dummy").unwrap();
        std::fs::write(outside.path().join("canary.txt"), b"synthetic-only").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("canary.txt"),
            root.path().join("link.txt"),
        )
        .unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("dir")).unwrap();
        std::fs::hard_link(
            outside.path().join("canary.txt"),
            root.path().join("alias.txt"),
        )
        .unwrap();
        assert!(open_no_follow(root.path(), "ok.txt").is_ok());
        assert!(open_no_follow(root.path(), "link.txt").is_err());
        assert!(open_no_follow(root.path(), "dir/canary.txt").is_err());
        assert!(open_no_follow(root.path(), "alias.txt").is_err());
    }
}
