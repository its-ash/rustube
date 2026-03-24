use axum::{
    Json as AxumJson, Router,
    extract::{Json, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader},
    process::Command,
    sync::RwLock,
    time::{Duration, sleep, timeout},
};
use tower_http::{services::ServeDir, trace::TraceLayer};
use tracing::{info, warn};
use url::Url;
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    jobs: Arc<RwLock<HashMap<String, DownloadJob>>>,
    download_dir: PathBuf,
}

const FILE_TTL_SECS: u64 = 24 * 60 * 60;
const CLEANUP_INTERVAL_SECS: u64 = 60 * 60;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rustube=info,tower_http=info".into()),
        )
        .init();

    let download_dir = PathBuf::from("downloads");
    tokio::fs::create_dir_all(&download_dir)
        .await
        .expect("failed to create downloads directory");

    let state = AppState {
        jobs: Arc::new(RwLock::new(HashMap::new())),
        download_dir: download_dir.clone(),
    };

    // Run one cleanup on startup and then periodically remove stale files/jobs.
    cleanup_old_files_and_jobs(&state).await;
    let cleanup_state = state.clone();
    tokio::spawn(async move {
        loop {
            sleep(Duration::from_secs(CLEANUP_INTERVAL_SECS)).await;
            cleanup_old_files_and_jobs(&cleanup_state).await;
        }
    });

    let app = Router::new()
        .route("/", get(index_handler))
        .route("/api/options", post(fetch_options))
        .route("/api/downloads/start", post(start_downloads))
        .route("/api/downloads/status", post(download_status))
        .route("/api/downloads/all", get(all_downloads))
        .route(
            "/api/downloads/clear-completed",
            post(clear_completed_downloads),
        )
        .route("/api/downloads/remove", post(remove_download_jobs))
        .nest_service("/downloads", ServeDir::new(download_dir))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], 3000));
    info!("Server running on http://{addr}");

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind server socket");
    axum::serve(listener, app)
        .await
        .expect("server failed unexpectedly");
}

async fn index_handler() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}

#[derive(Debug, Deserialize)]
struct OptionsRequest {
    url: String,
}

#[derive(Debug, Serialize)]
struct OptionsResponse {
    title: String,
    extractor: String,
    options: Vec<VideoOption>,
}

#[derive(Debug, Serialize, Clone)]
struct VideoOption {
    format_id: String,
    ext: String,
    resolution: String,
    size_bytes: Option<u64>,
    size_label: String,
    note: String,
    height: Option<u32>,
    media_kind: MediaKind,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum MediaKind {
    AudioVideo,
    AudioOnly,
    VideoOnly,
}

#[derive(Debug, Deserialize)]
struct StartDownloadsRequest {
    items: Vec<DownloadTaskRequest>,
}

#[derive(Debug, Deserialize)]
struct DownloadTaskRequest {
    url: String,
    format_id: String,
}

#[derive(Debug, Serialize)]
struct StartDownloadsResponse {
    jobs: Vec<DownloadJob>,
}

#[derive(Debug, Deserialize)]
struct DownloadStatusRequest {
    ids: Vec<String>,
}

#[derive(Debug, Serialize)]
struct DownloadStatusResponse {
    jobs: Vec<DownloadJob>,
}

#[derive(Debug, Deserialize)]
struct RemoveJobsRequest {
    ids: Vec<String>,
}

#[derive(Debug, Serialize)]
struct JobsMutationResponse {
    removed_jobs: usize,
    deleted_files: usize,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "snake_case")]
enum DownloadStatusKind {
    Queued,
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Serialize, Clone)]
struct DownloadJob {
    id: String,
    url: String,
    format_id: String,
    status: DownloadStatusKind,
    progress: f32,
    file_name: Option<String>,
    error: Option<String>,
    updated_at: u64,
}

#[derive(Debug, Clone)]
struct FallbackMedia {
    title: Option<String>,
    media_url: String,
    ext: String,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

struct AppError {
    status: StatusCode,
    message: String,
}

impl AppError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (
            self.status,
            AxumJson(ErrorResponse {
                error: self.message,
            }),
        )
            .into_response()
    }
}

async fn fetch_options(
    Json(payload): Json<OptionsRequest>,
) -> Result<AxumJson<OptionsResponse>, AppError> {
    let normalized_url = normalize_video_url(&payload.url)?;

    match run_yt_dlp_json(&normalized_url).await {
        Ok(metadata) => {
            let resolved_formats = metadata.resolved_formats();
            let mut options = extract_video_options(&resolved_formats);

            options.sort_by(|a, b| {
                b.height
                    .unwrap_or(0)
                    .cmp(&a.height.unwrap_or(0))
                    .then_with(|| b.size_bytes.unwrap_or(0).cmp(&a.size_bytes.unwrap_or(0)))
            });

            if options.is_empty() {
                return Err(AppError::bad_request(
                    "No downloadable formats were found for this URL.",
                ));
            }

            Ok(AxumJson(OptionsResponse {
                title: metadata
                    .resolved_title()
                    .unwrap_or_else(|| "Untitled video".to_string()),
                extractor: metadata
                    .resolved_extractor()
                    .unwrap_or_else(|| "unknown".to_string()),
                options,
            }))
        }
        Err(yt_err) => {
            if is_youtube_bot_error(&yt_err.message) {
                return Err(yt_err);
            }

            match extract_fallback_media(&normalized_url).await {
                Ok(media) => Ok(AxumJson(OptionsResponse {
                    title: media.title.unwrap_or_else(|| "Untitled video".to_string()),
                    extractor: "custom_rust_fallback".to_string(),
                    options: vec![VideoOption {
                        format_id: "best".to_string(),
                        ext: media.ext,
                        resolution: "Unknown".to_string(),
                        size_bytes: None,
                        size_label: "Unknown".to_string(),
                        note: "Direct media URL (custom Rust fallback)".to_string(),
                        height: None,
                        media_kind: MediaKind::AudioVideo,
                    }],
                })),
                Err(fallback_err) => Err(AppError::bad_request(format!(
                    "{} Fallback failed: {fallback_err}",
                    yt_err.message
                ))),
            }
        }
    }
}

async fn start_downloads(
    State(state): State<AppState>,
    Json(payload): Json<StartDownloadsRequest>,
) -> Result<AxumJson<StartDownloadsResponse>, AppError> {
    if payload.items.is_empty() {
        return Err(AppError::bad_request(
            "Please add at least one item to start downloads.",
        ));
    }

    if payload.items.len() > 20 {
        return Err(AppError::bad_request(
            "Maximum 20 downloads per request are allowed.",
        ));
    }

    let mut jobs = Vec::with_capacity(payload.items.len());

    for item in payload.items {
        let normalized_url = normalize_video_url(&item.url)?;
        validate_format_id(&item.format_id)?;

        let id = Uuid::new_v4().simple().to_string()[..12].to_string();
        let job = DownloadJob {
            id: id.clone(),
            url: normalized_url.clone(),
            format_id: item.format_id.clone(),
            status: DownloadStatusKind::Queued,
            progress: 0.0,
            file_name: None,
            error: None,
            updated_at: timestamp_secs(),
        };

        {
            let mut map = state.jobs.write().await;
            map.insert(id.clone(), job.clone());
        }

        spawn_download_worker(state.clone(), id, normalized_url, item.format_id);
        jobs.push(job);
    }

    Ok(AxumJson(StartDownloadsResponse { jobs }))
}

async fn download_status(
    State(state): State<AppState>,
    Json(payload): Json<DownloadStatusRequest>,
) -> Result<AxumJson<DownloadStatusResponse>, AppError> {
    if payload.ids.is_empty() {
        return Ok(AxumJson(DownloadStatusResponse { jobs: vec![] }));
    }

    let map = state.jobs.read().await;
    let mut jobs = Vec::with_capacity(payload.ids.len());

    for id in payload.ids {
        if let Some(job) = map.get(&id) {
            jobs.push(job.clone());
        }
    }

    Ok(AxumJson(DownloadStatusResponse { jobs }))
}

async fn all_downloads(
    State(state): State<AppState>,
) -> Result<AxumJson<DownloadStatusResponse>, AppError> {
    let map = state.jobs.read().await;
    let mut jobs: Vec<DownloadJob> = map.values().cloned().collect();
    jobs.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));

    Ok(AxumJson(DownloadStatusResponse { jobs }))
}

async fn clear_completed_downloads(
    State(state): State<AppState>,
) -> Result<AxumJson<JobsMutationResponse>, AppError> {
    let removed_jobs;
    let mut files_to_delete = Vec::<String>::new();

    {
        let mut map = state.jobs.write().await;
        let completed_ids: Vec<String> = map
            .iter()
            .filter_map(|(id, job)| {
                if matches!(job.status, DownloadStatusKind::Completed) {
                    Some(id.clone())
                } else {
                    None
                }
            })
            .collect();

        removed_jobs = completed_ids.len();
        for id in completed_ids {
            if let Some(job) = map.remove(&id) {
                if let Some(file_name) = job.file_name {
                    files_to_delete.push(file_name);
                }
            }
        }
    }

    let deleted_files = delete_files_by_name(&state.download_dir, &files_to_delete).await;

    Ok(AxumJson(JobsMutationResponse {
        removed_jobs,
        deleted_files,
    }))
}

async fn remove_download_jobs(
    State(state): State<AppState>,
    Json(payload): Json<RemoveJobsRequest>,
) -> Result<AxumJson<JobsMutationResponse>, AppError> {
    if payload.ids.is_empty() {
        return Ok(AxumJson(JobsMutationResponse {
            removed_jobs: 0,
            deleted_files: 0,
        }));
    }

    let mut removed_jobs = 0usize;
    let mut files_to_delete = Vec::<String>::new();
    {
        let mut map = state.jobs.write().await;
        for id in payload.ids {
            if let Some(job) = map.remove(&id) {
                removed_jobs = removed_jobs.saturating_add(1);
                if let Some(file_name) = job.file_name {
                    files_to_delete.push(file_name);
                }
            }
        }
    }

    let deleted_files = delete_files_by_name(&state.download_dir, &files_to_delete).await;

    Ok(AxumJson(JobsMutationResponse {
        removed_jobs,
        deleted_files,
    }))
}

fn spawn_download_worker(state: AppState, job_id: String, url: String, format_id: String) {
    tokio::spawn(async move {
        update_job(&state, &job_id, |job| {
            job.status = DownloadStatusKind::Running;
            job.progress = 0.0;
            job.error = None;
        })
        .await;

        let output_template = state
            .download_dir
            .join(format!("%(title).180B [%(id)s] [{job_id}].%(ext)s"))
            .to_string_lossy()
            .to_string();

        let cookie_scoop_file = maybe_extract_cookie_scoop_cookies(&url).await;

        let mut command = Command::new("yt-dlp");
        apply_common_yt_dlp_args(&mut command);
        apply_yt_dlp_auth_args(&mut command, None, cookie_scoop_file.as_deref());

        let mut child = match command
            .arg("--no-playlist")
            .arg("--newline")
            .arg("-f")
            .arg(&format_id)
            .arg("-o")
            .arg(&output_template)
            .arg(&url)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(err) => {
                let yt_error = map_yt_dlp_process_error(err);
                match attempt_custom_fallback_download(&state, &job_id, &url).await {
                    Ok(file_name) => {
                        update_job(&state, &job_id, |job| {
                            job.status = DownloadStatusKind::Completed;
                            job.progress = 100.0;
                            job.file_name = Some(file_name.clone());
                            job.error = None;
                        })
                        .await;
                    }
                    Err(fallback_error) => {
                        update_job(&state, &job_id, |job| {
                            job.status = DownloadStatusKind::Failed;
                            job.error = Some(format!(
                                "{yt_error}. Rust fallback failed: {fallback_error}"
                            ));
                        })
                        .await;
                    }
                }
                cleanup_cookie_file(cookie_scoop_file).await;
                return;
            }
        };

        let mut readers = Vec::new();
        if let Some(stderr) = child.stderr.take() {
            readers.push(spawn_progress_reader(state.clone(), job_id.clone(), stderr));
        }
        if let Some(stdout) = child.stdout.take() {
            readers.push(spawn_progress_reader(state.clone(), job_id.clone(), stdout));
        }

        let status_result = child.wait().await;

        for handle in readers {
            let _ = handle.await;
        }

        match status_result {
            Ok(status) if status.success() => {
                let file_name = find_downloaded_file(&state.download_dir, &job_id).await;
                if let Some(file_name) = file_name {
                    update_job(&state, &job_id, move |job| {
                        job.status = DownloadStatusKind::Completed;
                        job.progress = 100.0;
                        job.file_name = Some(file_name.clone());
                        job.error = None;
                    })
                    .await;
                } else {
                    match attempt_custom_fallback_download(&state, &job_id, &url).await {
                        Ok(file_name) => {
                            update_job(&state, &job_id, |job| {
                                job.status = DownloadStatusKind::Completed;
                                job.progress = 100.0;
                                job.file_name = Some(file_name.clone());
                                job.error = None;
                            })
                            .await;
                        }
                        Err(fallback_error) => {
                            update_job(&state, &job_id, |job| {
                                job.status = DownloadStatusKind::Failed;
                                job.error = Some(format!(
                                    "yt-dlp finished but output file was missing. Rust fallback failed: {fallback_error}"
                                ));
                            })
                            .await;
                        }
                    }
                }
            }
            Ok(status) => {
                let prior_error = get_job_error(&state, &job_id).await;
                let yt_error = prior_error
                    .unwrap_or_else(|| format!("Download process exited with status {status}"));

                if is_youtube_bot_error(&yt_error) {
                    let cookie_hint = if is_youtube_url(&url) {
                        if let Some(cookie_file) = cookie_scoop_file.as_deref() {
                            if !has_probable_youtube_auth_cookie(cookie_file).await {
                                " Captured cookies look unauthenticated for YouTube (missing SID/HSID/SSID/SAPISID)."
                            } else {
                                ""
                            }
                        } else {
                            ""
                        }
                    } else {
                        ""
                    };
                    update_job(&state, &job_id, |job| {
                        job.status = DownloadStatusKind::Failed;
                        job.error = Some(format!(
                            "{yt_error}.{cookie_hint} Configure authenticated cookies via RUSTUBE_COOKIES_FILE or RUSTUBE_COOKIES_HEADER."
                        ));
                    })
                    .await;
                } else {
                    match attempt_custom_fallback_download(&state, &job_id, &url).await {
                        Ok(file_name) => {
                            update_job(&state, &job_id, |job| {
                                job.status = DownloadStatusKind::Completed;
                                job.progress = 100.0;
                                job.file_name = Some(file_name.clone());
                                job.error = None;
                            })
                            .await;
                        }
                        Err(fallback_error) => {
                            update_job(&state, &job_id, |job| {
                                job.status = DownloadStatusKind::Failed;
                                job.error = Some(format!(
                                    "{yt_error}. Rust fallback failed: {fallback_error}"
                                ));
                            })
                            .await;
                        }
                    }
                }
            }
            Err(err) => {
                let yt_error = format!("Failed while waiting for download process: {err}");
                match attempt_custom_fallback_download(&state, &job_id, &url).await {
                    Ok(file_name) => {
                        update_job(&state, &job_id, |job| {
                            job.status = DownloadStatusKind::Completed;
                            job.progress = 100.0;
                            job.file_name = Some(file_name.clone());
                            job.error = None;
                        })
                        .await;
                    }
                    Err(fallback_error) => {
                        update_job(&state, &job_id, |job| {
                            job.status = DownloadStatusKind::Failed;
                            job.error = Some(format!(
                                "{yt_error}. Rust fallback failed: {fallback_error}"
                            ));
                        })
                        .await;
                    }
                }
            }
        }

        cleanup_cookie_file(cookie_scoop_file).await;
    });
}

fn spawn_progress_reader<R>(
    state: AppState,
    job_id: String,
    reader: R,
) -> tokio::task::JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();

        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(percent) = extract_percent(&line) {
                update_job(&state, &job_id, |job| {
                    if percent > job.progress {
                        job.progress = percent.min(99.9);
                    }
                })
                .await;
            }

            if line.contains("ERROR:") {
                let message = line.trim().to_string();
                update_job(&state, &job_id, |job| {
                    job.error = Some(message.clone());
                })
                .await;
                warn!("download {job_id} error line: {message}");
            }
        }
    })
}

async fn update_job<F>(state: &AppState, job_id: &str, mutator: F)
where
    F: FnOnce(&mut DownloadJob),
{
    let mut map = state.jobs.write().await;
    if let Some(job) = map.get_mut(job_id) {
        mutator(job);
        job.updated_at = timestamp_secs();
    }
}

async fn get_job_error(state: &AppState, job_id: &str) -> Option<String> {
    let map = state.jobs.read().await;
    map.get(job_id).and_then(|job| job.error.clone())
}

async fn cleanup_old_files_and_jobs(state: &AppState) {
    let now = timestamp_secs();
    let mut deleted_files = HashSet::<String>::new();

    if let Ok(mut entries) = tokio::fs::read_dir(&state.download_dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let Ok(meta) = entry.metadata().await else {
                continue;
            };

            if !meta.is_file() {
                continue;
            }

            let Ok(modified) = meta.modified() else {
                continue;
            };

            let age_secs = SystemTime::now()
                .duration_since(modified)
                .map(|d| d.as_secs())
                .unwrap_or(0);

            if age_secs > FILE_TTL_SECS {
                let file_name = entry.file_name().to_string_lossy().to_string();
                if tokio::fs::remove_file(entry.path()).await.is_ok() {
                    deleted_files.insert(file_name);
                }
            }
        }
    }

    let removed_jobs;
    {
        let mut map = state.jobs.write().await;

        for job in map.values_mut() {
            if let Some(name) = &job.file_name {
                if deleted_files.contains(name) {
                    job.file_name = None;
                    if job.error.is_none() {
                        job.error = Some("File auto-deleted (older than 1 day).".to_string());
                    }
                    job.updated_at = now;
                }
            }
        }

        let stale_ids: Vec<String> = map
            .iter()
            .filter_map(|(id, job)| {
                let is_terminal = matches!(
                    job.status,
                    DownloadStatusKind::Completed | DownloadStatusKind::Failed
                );
                let is_stale = now.saturating_sub(job.updated_at) > FILE_TTL_SECS;
                if is_terminal && is_stale {
                    Some(id.clone())
                } else {
                    None
                }
            })
            .collect();

        removed_jobs = stale_ids.len();
        for id in stale_ids {
            map.remove(&id);
        }
    }

    if !deleted_files.is_empty() || removed_jobs > 0 {
        info!(
            "cleanup complete: deleted {} old files, removed {} stale jobs",
            deleted_files.len(),
            removed_jobs
        );
    }
}

async fn delete_files_by_name(download_dir: &Path, files_to_delete: &[String]) -> usize {
    let mut deleted = 0usize;
    for file_name in files_to_delete {
        let path = download_dir.join(file_name);
        match tokio::fs::remove_file(&path).await {
            Ok(_) => {
                deleted = deleted.saturating_add(1);
            }
            Err(err) => {
                if err.kind() != std::io::ErrorKind::NotFound {
                    warn!("failed removing file {}: {}", path.to_string_lossy(), err);
                }
            }
        }
    }
    deleted
}

async fn attempt_custom_fallback_download(
    state: &AppState,
    job_id: &str,
    page_url: &str,
) -> Result<String, String> {
    let fallback = extract_fallback_media(page_url).await?;
    let file_stem = fallback
        .title
        .as_deref()
        .map(sanitize_filename)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "video".to_string());

    let file_name = format!("{file_stem} [{job_id}].{}", fallback.ext);
    let output_path = state.download_dir.join(&file_name);
    download_direct_media(state, job_id, page_url, &fallback.media_url, &output_path).await?;
    Ok(file_name)
}

async fn download_direct_media(
    state: &AppState,
    job_id: &str,
    page_url: &str,
    media_url: &str,
    output_path: &Path,
) -> Result<(), String> {
    let media_ext = infer_extension(media_url);
    if media_ext == "m3u8" || media_ext == "mpd" {
        update_job(state, job_id, |job| {
            job.progress = 30.0;
        })
        .await;
        download_adaptive_with_ffmpeg(media_url, output_path, page_url).await?;
        return Ok(());
    }

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(10))
        .timeout(Duration::from_secs(45))
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36")
        .build()
        .map_err(|err| format!("Failed to create HTTP client: {err}"))?;

    let origin = origin_from_url(page_url);
    let mut response =
        get_with_retries(&client, media_url, Some(page_url), origin.as_deref()).await?;

    let total = response.content_length();
    let mut file = tokio::fs::File::create(output_path)
        .await
        .map_err(|err| format!("Failed to create output file: {err}"))?;

    let mut downloaded: u64 = 0;
    loop {
        let next = response
            .chunk()
            .await
            .map_err(|err| format!("Failed while reading response body: {err}"))?;

        let Some(chunk) = next else {
            break;
        };

        file.write_all(&chunk)
            .await
            .map_err(|err| format!("Failed writing downloaded bytes: {err}"))?;
        downloaded = downloaded.saturating_add(chunk.len() as u64);

        if let Some(total_bytes) = total {
            if total_bytes > 0 {
                let percent = (downloaded as f32 / total_bytes as f32) * 100.0;
                update_job(state, job_id, |job| {
                    job.progress = percent.min(99.9);
                })
                .await;
            }
        }
    }

    file.flush()
        .await
        .map_err(|err| format!("Failed flushing output file: {err}"))?;

    Ok(())
}

async fn extract_fallback_media(page_url: &str) -> Result<FallbackMedia, String> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(10))
        .timeout(Duration::from_secs(30))
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36")
        .build()
        .map_err(|err| format!("Failed to create fallback HTTP client: {err}"))?;

    let response = get_with_retries(&client, page_url, None, None).await?;

    let base_url = response.url().clone();
    let html = response
        .text()
        .await
        .map_err(|err| format!("Failed to read page HTML: {err}"))?;

    let title = extract_page_title(&html);
    let candidates = extract_media_candidates(&html, &base_url);

    let Some((media_url, ext)) = candidates.into_iter().next() else {
        return Err("No direct media URL found on page".to_string());
    };

    Ok(FallbackMedia {
        title,
        media_url,
        ext,
    })
}

fn extract_page_title(html: &str) -> Option<String> {
    let og_title_re =
        Regex::new(r#"(?is)<meta[^>]+property=["']og:title["'][^>]+content=["']([^"']+)["']"#)
            .ok()?;
    if let Some(caps) = og_title_re.captures(html) {
        if let Some(m) = caps.get(1) {
            let value = html_decode_basic(m.as_str()).trim().to_string();
            if !value.is_empty() {
                return Some(value);
            }
        }
    }

    let title_re = Regex::new(r#"(?is)<title[^>]*>(.*?)</title>"#).ok()?;
    title_re.captures(html).and_then(|caps| {
        caps.get(1).map(|m| {
            html_decode_basic(m.as_str())
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
    })
}

fn extract_media_candidates(html: &str, base_url: &Url) -> Vec<(String, String)> {
    let mut raw_candidates = Vec::<String>::new();
    let patterns = [
        r#"(?is)<meta[^>]+property=["']og:video(?::url)?["'][^>]+content=["']([^"']+)["']"#,
        r#"(?is)<meta[^>]+itemprop=["']contentUrl["'][^>]+content=["']([^"']+)["']"#,
        r#"(?is)<video[^>]+src=["']([^"']+)["']"#,
        r#"(?is)<source[^>]+src=["']([^"']+)["']"#,
        r#"(?is)<source[^>]+data-src=["']([^"']+)["']"#,
        r#"(?is)"file"\s*:\s*"([^"]+)""#,
        r#"(?is)"(?:video_url|videoUrl|stream_url|streamUrl|hlsUrl|playlist|manifest)"\s*:\s*"([^"]+)""#,
        r#"(?is)"src"\s*:\s*"([^"]+?\.(?:mp4|webm|m3u8)[^"]*)""#,
        r#"(?is)https?:\\?/\\?/[^"' <>()]+?\.(?:mp4|webm|m3u8|mpd|m4a|mp3|mov|mkv)[^"' <>()]*"#,
    ];

    for pattern in patterns {
        if let Ok(re) = Regex::new(pattern) {
            for caps in re.captures_iter(html) {
                if caps.len() > 1 {
                    if let Some(m) = caps.get(1) {
                        raw_candidates.push(m.as_str().to_string());
                    }
                } else if let Some(m) = caps.get(0) {
                    raw_candidates.push(m.as_str().to_string());
                }
            }
        }
    }

    let mut seen = HashSet::new();
    let mut out = Vec::new();

    for raw in raw_candidates {
        let cleaned = unescape_candidate_url(&raw);
        let normalized = normalize_candidate_url(base_url, &cleaned);
        let Some(media_url) = normalized else {
            continue;
        };

        let ext = infer_extension(&media_url);
        let key = format!("{media_url}|{ext}");
        if seen.insert(key) {
            out.push((media_url, ext));
        }
    }

    // Prefer mp4/webm over m3u8 for easier direct download.
    out.sort_by_key(|(_, ext)| match ext.as_str() {
        "mp4" => 0,
        "webm" => 1,
        "m4a" => 2,
        "mp3" => 3,
        "m3u8" => 9,
        _ => 5,
    });
    out
}

fn unescape_candidate_url(raw: &str) -> String {
    raw.replace("\\/", "/")
        .replace("\\x2F", "/")
        .replace("\\u002F", "/")
        .replace("\\u0026", "&")
        .replace("\\u003A", ":")
        .replace("\\u003D", "=")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
}

fn normalize_candidate_url(base_url: &Url, value: &str) -> Option<String> {
    if value.starts_with("http://") || value.starts_with("https://") {
        return Some(value.to_string());
    }

    if value.starts_with("//") {
        return Some(format!("https:{value}"));
    }

    base_url.join(value).ok().and_then(|joined| {
        let scheme = joined.scheme();
        if scheme == "http" || scheme == "https" {
            Some(joined.to_string())
        } else {
            None
        }
    })
}

fn infer_extension(media_url: &str) -> String {
    let lower = media_url.to_ascii_lowercase();
    for ext in ["mp4", "webm", "m4a", "mp3", "m3u8", "mpd", "mov", "mkv"] {
        if lower.contains(&format!(".{ext}")) {
            return ext.to_string();
        }
    }
    "mp4".to_string()
}

fn origin_from_url(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    let scheme = parsed.scheme();
    let host = parsed.host_str()?;
    let mut origin = format!("{scheme}://{host}");
    if let Some(port) = parsed.port() {
        origin.push(':');
        origin.push_str(&port.to_string());
    }
    Some(origin)
}

async fn get_with_retries(
    client: &reqwest::Client,
    url: &str,
    referer: Option<&str>,
    origin: Option<&str>,
) -> Result<reqwest::Response, String> {
    let mut last_error = String::new();

    for attempt in 1..=3 {
        let mut request = client.get(url);
        if let Some(referer) = referer {
            request = request.header("Referer", referer);
        }
        if let Some(origin) = origin {
            request = request.header("Origin", origin);
        }

        match request.send().await {
            Ok(response) if response.status().is_success() => return Ok(response),
            Ok(response) => {
                last_error = format!("HTTP status {}", response.status());
                let retryable =
                    response.status().is_server_error() || response.status().as_u16() == 429;
                if retryable && attempt < 3 {
                    sleep(Duration::from_secs(attempt as u64)).await;
                    continue;
                }
                return Err(last_error);
            }
            Err(err) => {
                last_error = err.to_string();
                if attempt < 3 {
                    sleep(Duration::from_secs(attempt as u64)).await;
                    continue;
                }
            }
        }
    }

    Err(format!("Request failed: {last_error}"))
}

async fn download_adaptive_with_ffmpeg(
    media_url: &str,
    output_path: &Path,
    page_url: &str,
) -> Result<(), String> {
    let mut command = Command::new("ffmpeg");
    command
        .arg("-y")
        .arg("-loglevel")
        .arg("error")
        .arg("-headers")
        .arg(format!(
            "Referer: {page_url}\r\nUser-Agent: Mozilla/5.0\r\n"
        ))
        .arg("-i")
        .arg(media_url)
        .arg("-c")
        .arg("copy")
        .arg(output_path);

    let output = command
        .output()
        .await
        .map_err(|err| format!("ffmpeg execution failed: {err}"))?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        if stderr.is_empty() {
            Err("ffmpeg failed to download adaptive stream".to_string())
        } else {
            Err(format!("ffmpeg failed: {stderr}"))
        }
    }
}

fn html_decode_basic(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

fn extract_percent(line: &str) -> Option<f32> {
    let percent_index = line.find('%')?;
    let bytes = line.as_bytes();
    let mut start = percent_index;

    while start > 0 {
        let c = bytes[start - 1] as char;
        if c.is_ascii_digit() || c == '.' {
            start -= 1;
        } else {
            break;
        }
    }

    let number = line[start..percent_index].trim();
    if number.is_empty() {
        return None;
    }

    let value = number.parse::<f32>().ok()?;
    if (0.0..=100.0).contains(&value) {
        Some(value)
    } else {
        None
    }
}

async fn find_downloaded_file(download_dir: &Path, job_id: &str) -> Option<String> {
    let mut entries = tokio::fs::read_dir(download_dir).await.ok()?;
    let marker = format!("[{job_id}]");

    while let Some(entry) = entries.next_entry().await.ok().flatten() {
        let file_name = entry.file_name().to_string_lossy().to_string();
        if file_name.contains(&marker) && !file_name.ends_with(".part") {
            return Some(file_name);
        }
    }

    None
}

fn timestamp_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|dur| dur.as_secs())
        .unwrap_or(0)
}

fn normalize_video_url(raw: &str) -> Result<String, AppError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(AppError::bad_request("Please provide a valid URL."));
    }

    let mut candidate = trimmed.to_string();
    let mut parsed = Url::parse(&candidate).ok();
    if parsed.is_none() && !trimmed.contains("://") {
        candidate = format!("https://{trimmed}");
        parsed = Url::parse(&candidate).ok();
    }

    let parsed = parsed.ok_or_else(|| AppError::bad_request("Please provide a valid URL."))?;
    let scheme = parsed.scheme();

    if scheme != "http" && scheme != "https" {
        return Err(AppError::bad_request(
            "Only http:// or https:// video URLs are supported.",
        ));
    }

    if parsed.host_str().is_none() {
        return Err(AppError::bad_request("URL is missing a host name."));
    }

    Ok(candidate)
}

fn validate_format_id(format_id: &str) -> Result<(), AppError> {
    if format_id.is_empty() || format_id.len() > 80 {
        return Err(AppError::bad_request("Invalid format id."));
    }

    Ok(())
}

#[derive(Debug, Deserialize)]
struct YtDlpMetadata {
    title: Option<String>,
    extractor: Option<String>,
    #[serde(default)]
    formats: Option<Vec<YtDlpFormat>>,
    #[serde(default)]
    entries: Option<Vec<YtDlpEntry>>,
}

#[derive(Debug, Deserialize, Clone)]
struct YtDlpFormat {
    format_id: String,
    ext: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    resolution: Option<String>,
    format_note: Option<String>,
    filesize: Option<u64>,
    filesize_approx: Option<u64>,
    vcodec: Option<String>,
    acodec: Option<String>,
}

#[derive(Debug, Deserialize)]
struct YtDlpEntry {
    title: Option<String>,
    extractor: Option<String>,
    #[serde(default)]
    formats: Option<Vec<YtDlpFormat>>,
    format_id: Option<String>,
    ext: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    resolution: Option<String>,
    format_note: Option<String>,
    filesize: Option<u64>,
    filesize_approx: Option<u64>,
    vcodec: Option<String>,
    acodec: Option<String>,
    url: Option<String>,
}

impl YtDlpMetadata {
    fn resolved_title(&self) -> Option<String> {
        self.title.clone().or_else(|| {
            self.entries
                .as_ref()
                .and_then(|entries| entries.iter().find_map(|entry| entry.title.clone()))
        })
    }

    fn resolved_extractor(&self) -> Option<String> {
        self.extractor.clone().or_else(|| {
            self.entries
                .as_ref()
                .and_then(|entries| entries.iter().find_map(|entry| entry.extractor.clone()))
        })
    }

    fn resolved_formats(&self) -> Vec<YtDlpFormat> {
        if let Some(formats) = self.formats.as_ref().filter(|formats| !formats.is_empty()) {
            return formats.clone();
        }

        if let Some(entries) = self.entries.as_ref() {
            for entry in entries {
                if let Some(formats) = entry.formats.as_ref().filter(|formats| !formats.is_empty()) {
                    return formats.clone();
                }
            }

            let synthesized: Vec<YtDlpFormat> = entries
                .iter()
                .enumerate()
                .filter_map(|(idx, entry)| entry.to_single_format(idx))
                .collect();
            if !synthesized.is_empty() {
                return synthesized;
            }
        }

        Vec::new()
    }
}

impl YtDlpEntry {
    fn to_single_format(&self, index: usize) -> Option<YtDlpFormat> {
        let has_media_hint = self
            .url
            .as_deref()
            .map(|value| value.starts_with("http://") || value.starts_with("https://"))
            .unwrap_or(false)
            || self
                .ext
                .as_deref()
                .map(|value| !value.is_empty() && value != "none")
                .unwrap_or(false)
            || self
                .format_id
                .as_deref()
                .map(|value| !value.trim().is_empty())
                .unwrap_or(false);

        if !has_media_hint {
            return None;
        }

        Some(YtDlpFormat {
            format_id: self
                .format_id
                .clone()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| {
                    if index == 0 {
                        "best".to_string()
                    } else {
                        format!("best-{index}")
                    }
                }),
            ext: self.ext.clone(),
            width: self.width,
            height: self.height,
            resolution: self.resolution.clone(),
            format_note: self.format_note.clone(),
            filesize: self.filesize,
            filesize_approx: self.filesize_approx,
            vcodec: self.vcodec.clone(),
            acodec: self.acodec.clone(),
        })
    }
}

async fn run_yt_dlp_json(url: &str) -> Result<YtDlpMetadata, AppError> {
    let cookie_scoop_file = maybe_extract_cookie_scoop_cookies(url).await;
    let result = async {
        let primary = run_yt_dlp_json_once(url, None, cookie_scoop_file.as_deref()).await?;
        if primary.status.success() {
            return serde_json::from_slice::<YtDlpMetadata>(&primary.stdout)
                .map_err(|_| AppError::internal("Received invalid JSON from yt-dlp"));
        }

        let primary_stderr = String::from_utf8_lossy(&primary.stderr).trim().to_string();
        if is_youtube_bot_error(&primary_stderr) {
            let mut retry_errors = Vec::<String>::new();
            for browser in cookie_browser_candidates() {
                info!("Retrying yt-dlp metadata with cookies-from-browser={browser}");
                match run_yt_dlp_json_once(url, Some(&browser), cookie_scoop_file.as_deref()).await {
                    Ok(output) if output.status.success() => {
                        return serde_json::from_slice::<YtDlpMetadata>(&output.stdout).map_err(|_| {
                            AppError::internal("Received invalid JSON from yt-dlp after cookie retry")
                        });
                    }
                    Ok(output) => {
                        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                        if !stderr.is_empty() {
                            retry_errors.push(format!("{browser}: {stderr}"));
                        }
                    }
                    Err(err) => retry_errors.push(format!("{browser}: {}", err.message)),
                }
            }

            let mut message = build_yt_dlp_failure_message(&primary_stderr);
            if !retry_errors.is_empty() {
                message.push_str(" Cookie retries failed: ");
                message.push_str(&retry_errors.join(" | "));
            }
            if is_youtube_url(url) {
                if let Some(cookie_file) = cookie_scoop_file.as_deref() {
                    if !has_probable_youtube_auth_cookie(cookie_file).await {
                        message.push_str(
                            " Captured cookies look unauthenticated for YouTube (missing SID/HSID/SSID/SAPISID).",
                        );
                    }
                }
            }
            message.push_str(
                " Configure authenticated cookies via RUSTUBE_COOKIES_FILE or RUSTUBE_COOKIES_HEADER.",
            );
            return Err(AppError::bad_request(message));
        }

        let message = if primary_stderr.is_empty() {
            "Failed to read video information via yt-dlp.".to_string()
        } else {
            build_yt_dlp_failure_message(&primary_stderr)
        };

        Err(AppError::bad_request(message))
    }
    .await;

    cleanup_cookie_file(cookie_scoop_file).await;
    result
}

async fn run_yt_dlp_json_once(
    url: &str,
    cookie_browser_override: Option<&str>,
    cookies_file_override: Option<&Path>,
) -> Result<std::process::Output, AppError> {
    let mut command = Command::new("yt-dlp");
    apply_common_yt_dlp_args(&mut command);
    apply_yt_dlp_auth_args(&mut command, cookie_browser_override, cookies_file_override);
    command
        .arg("--dump-single-json")
        .arg("--no-playlist")
        .arg(url)
        .output()
        .await
        .map_err(|err| AppError::internal(map_yt_dlp_process_error(err)))
}

fn map_yt_dlp_process_error(err: std::io::Error) -> String {
    if err.kind() == std::io::ErrorKind::NotFound {
        "`yt-dlp` was not found. Install it and ensure it is available in PATH.".to_string()
    } else {
        format!("Failed to start yt-dlp: {err}")
    }
}

fn apply_common_yt_dlp_args(command: &mut Command) {
    command
        .arg("--no-warnings")
        .arg("--geo-bypass")
        .arg("--extractor-retries")
        .arg("3")
        .arg("--retries")
        .arg("3")
        .arg("--fragment-retries")
        .arg("3")
        .arg("--retry-sleep")
        .arg("2")
        .arg("--socket-timeout")
        .arg("30")
        .arg("--user-agent")
        .arg("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36");
}

fn apply_yt_dlp_auth_args(
    command: &mut Command,
    cookie_browser_override: Option<&str>,
    cookies_file_override: Option<&Path>,
) {
    if let Some(cookie_browser) = cookie_browser_override {
        let cookie_browser = cookie_browser.trim();
        if !cookie_browser.is_empty() {
            command.arg("--cookies-from-browser").arg(cookie_browser);
        }
    } else if let Ok(browser) = std::env::var("RUSTUBE_COOKIES_FROM_BROWSER") {
        let browser = browser
            .split(',')
            .map(str::trim)
            .find(|item| !item.is_empty());
        if let Some(browser) = browser {
            command.arg("--cookies-from-browser").arg(browser);
        }
    }

    if let Some(cookies_file) = cookies_file_override {
        let cookies_file = cookies_file.to_string_lossy();
        let cookies_file = cookies_file.trim();
        if !cookies_file.is_empty() {
            command.arg("--cookies").arg(cookies_file);
        }
        return;
    }

    if let Ok(cookies_file) = std::env::var("RUSTUBE_COOKIES_FILE") {
        let cookies_file = cookies_file.trim();
        if !cookies_file.is_empty() {
            command.arg("--cookies").arg(cookies_file);
        }
    }
}

async fn maybe_extract_cookie_scoop_cookies(page_url: &str) -> Option<PathBuf> {
    if let Some(path) = maybe_extract_inline_cookie_file(page_url).await {
        return Some(path);
    }

    if !is_cookie_scoop_enabled() {
        return None;
    }

    let cookie_scoop_bin =
        std::env::var("RUSTUBE_COOKIE_SCOOP_BIN").unwrap_or_else(|_| "cookie-scoop".to_string());

    let temp_dir = std::env::temp_dir().join("rustube-cookie-scoop");
    if let Err(err) = tokio::fs::create_dir_all(&temp_dir).await {
        warn!("Failed to create cookie-scoop temp directory: {err}");
        return None;
    }

    let output_path = temp_dir.join(format!("cookies-{}.txt", Uuid::new_v4().simple()));
    let attempts = cookie_scoop_argument_attempts(page_url);
    let timeout_secs = std::env::var("RUSTUBE_COOKIE_SCOOP_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(20);

    let mut errors = Vec::<String>::new();

    for args in attempts {
        let mut command = Command::new(&cookie_scoop_bin);
        command.args(&args);

        let output = match timeout(Duration::from_secs(timeout_secs), command.output()).await {
            Ok(result) => result,
            Err(_) => {
                errors.push(format!("timed out after {timeout_secs}s"));
                continue;
            }
        };

        match output {
            Ok(output) if output.status.success() => {
                let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if write_cookie_scoop_output_file(&stdout, page_url, &output_path)
                    .await
                    .is_ok()
                    && path_exists_and_non_empty(&output_path).await
                {
                    info!("cookie-scoop cookies captured for {page_url}");
                    return Some(output_path);
                }

                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                let reason = if stderr.is_empty() {
                    "completed but did not emit parseable cookies".to_string()
                } else {
                    format!("completed but cookies were not parseable ({stderr})")
                };
                errors.push(reason);
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                let error_text = if stderr.is_empty() {
                    format!("exit {}", output.status)
                } else {
                    stderr
                };
                errors.push(error_text);
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                info!(
                    "cookie-scoop is not installed (set RUSTUBE_COOKIE_SCOOP_BIN or install cookie-scoop)"
                );
                return None;
            }
            Err(err) => {
                errors.push(format!("failed to start cookie-scoop: {err}"));
            }
        }
    }

    if !errors.is_empty() {
        warn!("cookie-scoop did not produce cookies for {page_url}: {}", errors.join(" | "));
    }
    let _ = tokio::fs::remove_file(&output_path).await;
    None
}

async fn maybe_extract_inline_cookie_file(page_url: &str) -> Option<PathBuf> {
    let inline = std::env::var("RUSTUBE_COOKIES_HEADER")
        .ok()
        .or_else(|| std::env::var("RUSTUBE_YOUTUBE_COOKIE_HEADER").ok())?;
    let inline = inline.trim();
    if inline.is_empty() {
        return None;
    }

    let content = parse_cookie_header_to_netscape(inline, page_url)?;
    let temp_dir = std::env::temp_dir().join("rustube-inline-cookies");
    if tokio::fs::create_dir_all(&temp_dir).await.is_err() {
        return None;
    }

    let output_path = temp_dir.join(format!("inline-{}.txt", Uuid::new_v4().simple()));
    if tokio::fs::write(&output_path, content.as_bytes()).await.is_ok() {
        info!("Using inline cookie header from env for {page_url}");
        Some(output_path)
    } else {
        None
    }
}

fn cookie_scoop_argument_attempts(page_url: &str) -> Vec<Vec<String>> {
    let mut base = vec!["--url".to_string(), page_url.to_string()];
    if let Some(browsers) = cookie_scoop_browsers() {
        base.push("--browsers".to_string());
        base.push(browsers);
    }

    let mut with_header = base.clone();
    with_header.push("--header".to_string());

    vec![with_header, base]
}

fn best_cookie_domain_for_host(host: &str) -> String {
    let host = host.trim().trim_start_matches('.');
    if host.is_empty() {
        return "localhost".to_string();
    }

    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() >= 2 {
        format!(".{}.{}", labels[labels.len() - 2], labels[labels.len() - 1])
    } else {
        host.to_string()
    }
}

fn is_youtube_url(url: &str) -> bool {
    Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
        .map(|host| host.contains("youtube.com") || host == "youtu.be")
        .unwrap_or(false)
}

async fn has_probable_youtube_auth_cookie(cookie_file: &Path) -> bool {
    let Ok(content) = tokio::fs::read_to_string(cookie_file).await else {
        return false;
    };

    let keys = ["\tSID\t", "\tHSID\t", "\tSSID\t", "\tSAPISID\t", "\tLOGIN_INFO\t"];
    keys.iter().any(|needle| content.contains(needle))
}

fn is_cookie_scoop_enabled() -> bool {
    env_truthy("RUSTUBE_COOKIE_SCOOP_ENABLED", true)
}

fn looks_like_cookie_file(content: &str) -> bool {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.contains("# Netscape HTTP Cookie File") {
        return true;
    }

    trimmed.lines().any(|line| {
        if line.starts_with('#') {
            return false;
        }
        line.split('\t').count() >= 7
    })
}

fn cookie_scoop_browsers() -> Option<String> {
    std::env::var("RUSTUBE_COOKIE_SCOOP_BROWSERS")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            std::env::var("RUSTUBE_COOKIES_FROM_BROWSER")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
}

fn env_truthy(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        Ok(value) => {
            let value = value.trim().to_ascii_lowercase();
            !(value == "0" || value == "false" || value == "off" || value == "no")
        }
        Err(_) => default,
    }
}

async fn write_cookie_scoop_output_file(
    stdout: &str,
    page_url: &str,
    output_path: &Path,
) -> Result<(), String> {
    let content = parse_cookie_scoop_output_to_netscape(stdout, page_url)
        .ok_or_else(|| "cookie-scoop output was not recognized".to_string())?;
    tokio::fs::write(output_path, content.as_bytes())
        .await
        .map_err(|err| format!("Failed to write cookie-scoop cookie file: {err}"))
}

fn parse_cookie_scoop_output_to_netscape(stdout: &str, page_url: &str) -> Option<String> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return None;
    }

    if looks_like_cookie_file(trimmed) {
        return Some(trimmed.to_string());
    }

    if let Some(cookie_header) = parse_cookie_header_to_netscape(trimmed, page_url) {
        return Some(cookie_header);
    }

    parse_cookie_json_to_netscape(trimmed, page_url)
}

fn parse_cookie_header_to_netscape(stdout: &str, page_url: &str) -> Option<String> {
    let mut header = stdout.trim();
    if let Some(stripped) = header
        .strip_prefix("Cookie:")
        .or_else(|| header.strip_prefix("cookie:"))
    {
        header = stripped.trim();
    }

    let parsed_url = Url::parse(page_url).ok()?;
    let host = parsed_url.host_str()?;
    let cookie_domain = best_cookie_domain_for_host(host);
    let secure = parsed_url.scheme() == "https";
    let include_subdomains = if cookie_domain.starts_with('.') {
        "TRUE"
    } else {
        "FALSE"
    };

    let mut lines = vec!["# Netscape HTTP Cookie File".to_string()];
    let mut found_any = false;

    for chunk in header.split(';') {
        let cookie = chunk.trim();
        if cookie.is_empty() {
            continue;
        }

        let mut parts = cookie.splitn(2, '=');
        let Some(name) = parts.next().map(str::trim).filter(|name| !name.is_empty()) else {
            continue;
        };
        let value = parts.next().unwrap_or("").trim();

        lines.push(format!(
            "{cookie_domain}\t{include_subdomains}\t/\t{}\t0\t{}\t{}",
            if secure { "TRUE" } else { "FALSE" },
            sanitize_cookie_field(name),
            sanitize_cookie_field(value)
        ));
        found_any = true;
    }

    if found_any {
        Some(lines.join("\n"))
    } else {
        None
    }
}

fn parse_cookie_json_to_netscape(stdout: &str, page_url: &str) -> Option<String> {
    let parsed = serde_json::from_str::<Value>(stdout).ok()?;
    let parsed_url = Url::parse(page_url).ok()?;
    let default_host = parsed_url.host_str()?;
    let default_secure = parsed_url.scheme() == "https";

    let items = if let Some(items) = parsed.as_array() {
        items.to_vec()
    } else if let Some(items) = parsed.get("cookies").and_then(Value::as_array) {
        items.to_vec()
    } else {
        return None;
    };

    let mut lines = vec!["# Netscape HTTP Cookie File".to_string()];
    let mut count = 0usize;

    for item in items {
        let Some(obj) = item.as_object() else {
            continue;
        };

        let Some(name) = obj
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
        else {
            continue;
        };

        let value = obj
            .get("value")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let domain = obj
            .get("domain")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(default_host);
        let include_subdomains = if domain.starts_with('.') {
            "TRUE"
        } else {
            "FALSE"
        };
        let path = obj
            .get("path")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("/");
        let secure = obj
            .get("secure")
            .and_then(Value::as_bool)
            .unwrap_or(default_secure);
        let expiry = obj
            .get("expires")
            .and_then(value_to_i64)
            .or_else(|| obj.get("expirationDate").and_then(value_to_i64))
            .or_else(|| obj.get("expiry").and_then(value_to_i64))
            .unwrap_or(0);

        lines.push(format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            sanitize_cookie_field(domain),
            include_subdomains,
            sanitize_cookie_field(path),
            if secure { "TRUE" } else { "FALSE" },
            if expiry > 0 { expiry } else { 0 },
            sanitize_cookie_field(name),
            sanitize_cookie_field(value)
        ));
        count = count.saturating_add(1);
    }

    if count == 0 {
        return None;
    }

    Some(lines.join("\n"))
}

fn value_to_i64(value: &Value) -> Option<i64> {
    if let Some(v) = value.as_i64() {
        return Some(v);
    }
    if let Some(v) = value.as_f64() {
        return Some(v as i64);
    }
    if let Some(v) = value.as_str() {
        if let Ok(parsed) = v.parse::<i64>() {
            return Some(parsed);
        }
        if let Ok(parsed) = v.parse::<f64>() {
            return Some(parsed as i64);
        }
    }
    None
}

fn sanitize_cookie_field(value: &str) -> String {
    value
        .replace('\t', " ")
        .replace('\r', "")
        .replace('\n', "")
}

async fn path_exists_and_non_empty(path: &Path) -> bool {
    match tokio::fs::metadata(path).await {
        Ok(meta) => meta.is_file() && meta.len() > 0,
        Err(_) => false,
    }
}

async fn cleanup_cookie_file(path: Option<PathBuf>) {
    let Some(path) = path else {
        return;
    };

    if let Err(err) = tokio::fs::remove_file(&path).await {
        if err.kind() != std::io::ErrorKind::NotFound {
            warn!("Failed to remove temporary cookie file {}: {err}", path.display());
        }
    }
}

fn cookie_browser_candidates() -> Vec<String> {
    let mut out = Vec::<String>::new();
    let mut seen = HashSet::<String>::new();

    if let Ok(from_env) = std::env::var("RUSTUBE_COOKIES_FROM_BROWSER") {
        for browser in from_env.split(',').map(str::trim).filter(|item| !item.is_empty()) {
            let value = browser.to_string();
            if seen.insert(value.clone()) {
                out.push(value);
            }
        }
    }

    if out.is_empty() && env_truthy("RUSTUBE_AUTO_COOKIE_BROWSER_RETRY", false) {
        for browser in ["chrome", "chromium", "brave", "edge", "firefox", "safari"] {
            let value = browser.to_string();
            if seen.insert(value.clone()) {
                out.push(value);
            }
        }
    }

    out
}

fn is_youtube_bot_error(text: &str) -> bool {
    let lowered = text.to_ascii_lowercase().replace('’', "'");
    lowered.contains("sign in to confirm you're not a bot")
        || lowered.contains("sign in to confirm you’re not a bot")
        || lowered.contains("use --cookies-from-browser or --cookies")
}

fn build_yt_dlp_failure_message(stderr: &str) -> String {
    let lowered = stderr.to_ascii_lowercase();
    let looks_like_site_block = lowered.contains("connection reset")
        || lowered.contains("403")
        || lowered.contains("forbidden")
        || lowered.contains("429")
        || lowered.contains("timed out");

    if looks_like_site_block {
        format!(
            "yt-dlp error: {stderr}. This site may be blocking automated requests right now. Try again later or use browser cookies."
        )
    } else {
        format!("yt-dlp error: {stderr}")
    }
}

fn extract_video_options(formats: &[YtDlpFormat]) -> Vec<VideoOption> {
    let mut seen_ids = HashSet::new();
    let mut options = Vec::new();

    for format in formats {
        if format.format_id.is_empty() || seen_ids.contains(&format.format_id) {
            continue;
        }

        let no_video = format.vcodec.as_deref() == Some("none");
        let no_audio = format.acodec.as_deref() == Some("none");
        if no_video && no_audio {
            continue;
        }

        let ext = format.ext.clone().unwrap_or_else(|| "bin".to_string());
        if ext == "mhtml" {
            continue;
        }

        let resolution = match (format.width, format.height) {
            (Some(w), Some(h)) => format!("{w}x{h}"),
            _ => format
                .resolution
                .clone()
                .or_else(|| format.format_note.clone())
                .unwrap_or_else(|| "Unknown".to_string()),
        };

        let size_bytes = format.filesize.or(format.filesize_approx);

        let media_kind = match (no_video, no_audio) {
            (true, false) => MediaKind::AudioOnly,
            (false, true) => MediaKind::VideoOnly,
            _ => MediaKind::AudioVideo,
        };

        let note = build_note(format);
        let option = VideoOption {
            format_id: format.format_id.clone(),
            ext,
            resolution,
            size_label: human_size(size_bytes),
            size_bytes,
            note,
            height: format.height,
            media_kind,
        };

        seen_ids.insert(format.format_id.clone());
        options.push(option);
    }

    // Many providers (especially YouTube) split high-quality video/audio.
    // Add synthetic "video+bestaudio" merge options so users can download AV together.
    append_merged_av_options(formats, &mut seen_ids, &mut options);

    options
}

fn append_merged_av_options(
    formats: &[YtDlpFormat],
    seen_ids: &mut HashSet<String>,
    options: &mut Vec<VideoOption>,
) {
    let best_audio_size = formats
        .iter()
        .filter(|f| f.vcodec.as_deref() == Some("none") && f.acodec.as_deref() != Some("none"))
        .filter_map(|f| f.filesize.or(f.filesize_approx))
        .max();

    for format in formats {
        let no_video = format.vcodec.as_deref() == Some("none");
        let no_audio = format.acodec.as_deref() == Some("none");

        // Pick video-only streams and pair with bestaudio.
        if no_video || !no_audio {
            continue;
        }

        let ext = format.ext.clone().unwrap_or_else(|| "bin".to_string());
        if ext == "mhtml" {
            continue;
        }

        let merged_id = format!("{}+bestaudio/best", format.format_id);
        if seen_ids.contains(&merged_id) {
            continue;
        }

        let resolution = match (format.width, format.height) {
            (Some(w), Some(h)) => format!("{w}x{h}"),
            _ => format
                .resolution
                .clone()
                .or_else(|| format.format_note.clone())
                .unwrap_or_else(|| "Unknown".to_string()),
        };

        let size_bytes = match (format.filesize.or(format.filesize_approx), best_audio_size) {
            (Some(video_size), Some(audio_size)) => Some(video_size.saturating_add(audio_size)),
            (Some(video_size), None) => Some(video_size),
            _ => None,
        };

        let option = VideoOption {
            format_id: merged_id.clone(),
            ext: ext.clone(),
            resolution,
            size_label: human_size(size_bytes),
            size_bytes,
            note: "video + best audio (merged)".to_string(),
            height: format.height,
            media_kind: MediaKind::AudioVideo,
        };

        seen_ids.insert(merged_id);
        options.push(option);
    }
}

fn build_note(format: &YtDlpFormat) -> String {
    let base = format
        .format_note
        .clone()
        .unwrap_or_else(|| "standard".to_string());

    match (
        format.vcodec.as_deref() == Some("none"),
        format.acodec.as_deref() == Some("none"),
    ) {
        (true, false) => format!("{base} (audio only)"),
        (false, true) => format!("{base} (video only)"),
        _ => base,
    }
}

fn human_size(size_bytes: Option<u64>) -> String {
    let Some(size_bytes) = size_bytes else {
        return "Unknown".to_string();
    };

    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut value = size_bytes as f64;
    let mut idx = 0;

    while value >= 1024.0 && idx < units.len() - 1 {
        value /= 1024.0;
        idx += 1;
    }

    format!("{value:.2} {}", units[idx])
}

fn sanitize_filename(title: &str) -> String {
    let mut out = String::with_capacity(title.len());

    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == ' ' {
            out.push(ch);
        } else if ch.is_whitespace() {
            out.push(' ');
        }
    }

    let trimmed = out.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = trimmed.trim();
    if trimmed.is_empty() {
        "video".to_string()
    } else {
        trimmed.chars().take(160).collect()
    }
}
