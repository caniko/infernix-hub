//! Fixture Windmill: runs the two registered procedures in-process with the
//! same contract as `registry/windmill/*.ts`; receipts and dispatch go over the
//! hub's HTTP API.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use chrono::Utc;
use contracts::{AttemptKind, DispatchIn, ReceiptIn};
use domain::ports::*;
use serde_json::{Value, json};

use super::{FixtureComfy, lock};

#[derive(Clone)]
struct HubLink {
    url: String,
    token: String,
}

#[derive(Clone)]
pub struct FixtureWindmill(Arc<WindmillState>);

impl std::ops::Deref for FixtureWindmill {
    type Target = WindmillState;
    fn deref(&self) -> &WindmillState {
        &self.0
    }
}

pub struct WindmillState {
    comfy: Arc<FixtureComfy>,
    hub: OnceLock<HubLink>,
    jobs: Mutex<HashMap<String, (JobStatus, Option<tokio::task::AbortHandle>)>>,
    /// Drop the procedure's receipt POSTs (lost submission response).
    pub drop_receipts: std::sync::atomic::AtomicBool,
    /// Do not arm timers automatically (tests fire them by hand).
    pub manual_timers: std::sync::atomic::AtomicBool,
    /// Record pipeline submissions but upload and receipt nothing: the test
    /// drives the artifact protocol by hand.
    pub manual_pipeline: std::sync::atomic::AtomicBool,
    /// The next pipeline status read reports a deferral: a run that ended
    /// without doing the work.
    pub defer_next_status: std::sync::atomic::AtomicBool,
    /// Pause the next pipeline submission after its run exists, until the test
    /// releases it: a cancel can then land while the handle is in flight.
    pub pause_submit: std::sync::atomic::AtomicBool,
    /// Signalled once a paused submission holds its run.
    pub submit_started: tokio::sync::Notify,
    /// Releases a paused submission.
    pub submit_release: tokio::sync::Notify,
    /// Hold the next pipeline status read forever (a backend that stops
    /// answering): only the caller's own bound can end the pass.
    pub slow_status: std::sync::atomic::AtomicBool,
    /// Signalled once a slow status read is parked.
    pub status_started: tokio::sync::Notify,
    /// Report a completed run whose manifest digest is invalid: a finished job
    /// with unusable terminal output, not a transient read failure.
    pub bad_manifest: std::sync::atomic::AtomicBool,
    /// Fail this many pipeline submissions as unreachable (a backend that is
    /// down), spending outbox retries.
    pub fail_submit: std::sync::atomic::AtomicUsize,
    timers: Mutex<Vec<PublicationTimer>>,
}

impl FixtureWindmill {
    pub fn new(comfy: Arc<FixtureComfy>) -> Self {
        Self(Arc::new(WindmillState {
            comfy,
            hub: OnceLock::new(),
            jobs: Mutex::default(),
            drop_receipts: Default::default(),
            manual_timers: Default::default(),
            manual_pipeline: Default::default(),
            defer_next_status: Default::default(),
            pause_submit: Default::default(),
            submit_started: tokio::sync::Notify::new(),
            submit_release: tokio::sync::Notify::new(),
            slow_status: Default::default(),
            status_started: tokio::sync::Notify::new(),
            bad_manifest: Default::default(),
            fail_submit: Default::default(),
            timers: Mutex::default(),
        }))
    }

    /// Point the procedures at the running hub (procedure token).
    pub fn connect(&self, hub_url: &str, procedure_token: &str) {
        let _ = self.hub.set(HubLink { url: hub_url.trim_end_matches('/').to_string(), token: procedure_token.to_string() });
    }

    fn client(&self) -> Option<contracts::client::HubClient> {
        self.hub.get().map(|h| contracts::client::HubClient::new(&h.url, &h.token))
    }

    fn new_job(&self, handle: Option<tokio::task::AbortHandle>) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        lock(&self.jobs).insert(id.clone(), (JobStatus::Running, handle));
        id
    }

    fn finish(&self, id: &str, status: JobStatus) {
        if let Some(j) = lock(&self.jobs).get_mut(id)
            && j.0 == JobStatus::Running
        {
            j.0 = status;
        }
    }

    /// Timers armed in manual mode, oldest first.
    pub fn armed_timers(&self) -> Vec<PublicationTimer> {
        lock(&self.timers).clone()
    }

    /// Fire a timer now, as the Windmill publication procedure would.
    pub async fn fire(&self, timer: &PublicationTimer) -> Result<contracts::DispatchOut, contracts::client::ClientError> {
        let client = self.client().expect("fixture windmill not connected");
        let path = timer.dispatch_url.split("/v1").nth(1).unwrap_or_default().to_string();
        client
            .post(
                &path,
                &DispatchIn { schedule_revision: timer.schedule_revision, trigger_id: format!("fixture-timer-{}", uuid::Uuid::new_v4()) },
            )
            .await
    }
}

#[async_trait]
impl Windmill for FixtureWindmill {
    async fn run_generation(&self, job: &GenerationJob) -> PortResult<String> {
        let client = self.client().ok_or_else(|| PortError::Unreachable("fixture windmill not connected".into()))?;
        let me = self.clone();
        let job = job.clone();
        let id = self.new_job(None);
        let job_id = id.clone();
        let task = tokio::spawn(async move {
            let receipt_path = job.receipt_url.split("/v1").nth(1).unwrap_or_default().to_string();
            for (prompt, pid) in job.prompts.iter().zip(&job.prompt_ids) {
                let actual = me.comfy.submit(pid, &job.comfy_client_id, prompt.clone());
                if me.drop_receipts.load(std::sync::atomic::Ordering::SeqCst) {
                    continue;
                }
                let receipt = ReceiptIn {
                    kind: AttemptKind::ComfyPrompt,
                    external_id: actual,
                    parent_external_id: Some(job_id.clone()),
                    engine_instance_id: Some(job.engine_instance_id.clone()),
                    status: None,
                    artifacts: Vec::new(),
                    raw: json!({ "fixture": true, "prompt_id": pid }),
                };
                // Bounded retries live inside the procedure.
                for attempt in 0..3 {
                    match client.post::<_, Value>(&receipt_path, &receipt).await {
                        Ok(_) => break,
                        Err(contracts::client::ClientError::Api { status: 409, .. }) => break,
                        Err(_) => tokio::time::sleep(std::time::Duration::from_millis(200 << attempt)).await,
                    }
                }
            }
            me.finish(&job_id, JobStatus::Succeeded);
        });
        if let Some(j) = lock(&self.jobs).get_mut(&id) {
            j.1 = Some(task.abort_handle());
        }
        Ok(id)
    }

    async fn schedule_publication(&self, timer: &PublicationTimer) -> PortResult<String> {
        if self.manual_timers.load(std::sync::atomic::Ordering::SeqCst) {
            lock(&self.timers).push(timer.clone());
            return Ok(self.new_job(None));
        }
        let me = self.clone();
        let timer = timer.clone();
        let id = self.new_job(None);
        let job_id = id.clone();
        let task = tokio::spawn(async move {
            let wait = (timer.due_at - Utc::now()).to_std().unwrap_or_default();
            tokio::time::sleep(wait).await;
            let status = match me.fire(&timer).await {
                Ok(_) => JobStatus::Succeeded,
                // Stale revisions and converged duplicates are a no-op success.
                Err(contracts::client::ClientError::Api { status: 409, .. }) => JobStatus::Succeeded,
                Err(_) => JobStatus::Failed,
            };
            me.finish(&job_id, status);
        });
        if let Some(j) = lock(&self.jobs).get_mut(&id) {
            j.1 = Some(task.abort_handle());
        }
        Ok(id)
    }

    async fn job(&self, id: &str) -> PortResult<JobView> {
        let jobs = lock(&self.jobs);
        let (status, _) = jobs.get(id).ok_or(PortError::NotFound)?;
        Ok(JobView { id: id.into(), status: *status, raw: json!({ "fixture": true }) })
    }

    async fn cancel_job(&self, id: &str) -> PortResult<()> {
        let mut jobs = lock(&self.jobs);
        let j = jobs.get_mut(id).ok_or(PortError::NotFound)?;
        if let Some(h) = j.1.take() {
            h.abort();
        }
        if j.0 == JobStatus::Running {
            j.0 = JobStatus::Cancelled;
        }
        Ok(())
    }
}

// ── Pipeline ────────────────────────────────────────────────────────────────

/// Fixture pipeline: stands in for a registered pipeline procedure. It renders
/// deterministic bytes, uploads them through the hub's artifact route and posts
/// the terminal manifest receipt, so a pipeline generation exercises the same
/// paths a live Windmill procedure does. It shares the windmill's hub link, so
/// `FixtureWindmill::connect` points it at the running server.
#[derive(Clone)]
pub struct FixturePipeline(Arc<FixturePipelineState>);

pub struct FixturePipelineState {
    windmill: FixtureWindmill,
    http: reqwest::Client,
    runs: Mutex<HashMap<String, (PipelineStatus, Option<tokio::task::AbortHandle>)>>,
}

impl FixturePipeline {
    pub fn new(windmill: FixtureWindmill) -> Self {
        Self(Arc::new(FixturePipelineState { windmill, http: reqwest::Client::new(), runs: Mutex::default() }))
    }

    fn set(&self, id: &str, status: PipelineStatus) {
        if let Some(r) = lock(&self.0.runs).get_mut(id) {
            r.0 = status;
        }
    }

    /// One rendered artifact plus its manifest entry, or the upload error.
    async fn upload(&self, url: &str, token: &str, artifact: &contracts::ArtifactRef, bytes: &[u8]) -> Result<(), String> {
        let resp = self
            .0
            .http
            .post(url)
            .bearer_auth(token)
            .header("x-artifact-source", &artifact.source)
            .header(reqwest::header::CONTENT_TYPE, &artifact.mime)
            .header("x-sha256", &artifact.sha256)
            .body(bytes.to_vec())
            .send()
            .await
            .map_err(|e| e.to_string())?;
        match resp.status().is_success() {
            true => Ok(()),
            false => Err(format!("uploading {}: HTTP {}", artifact.source, resp.status())),
        }
    }
}

fn artifact(source: &str, mime: &str, bytes: &[u8]) -> (contracts::ArtifactRef, Vec<u8>) {
    (
        contracts::ArtifactRef {
            source: source.into(),
            sha256: domain::hash::sha256_hex(bytes),
            mime: mime.into(),
            size: bytes.len() as u64,
        },
        bytes.to_vec(),
    )
}

#[async_trait]
impl Pipeline for FixturePipeline {
    async fn submit(&self, req: &PipelineRequest) -> PortResult<PipelineRun> {
        let Some(link) = self.0.windmill.hub.get().cloned() else {
            return Err(PortError::Unreachable("fixture pipeline not connected".into()));
        };
        if self.0.windmill.fail_submit.load(std::sync::atomic::Ordering::SeqCst) > 0 {
            self.0.windmill.fail_submit.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            return Err(PortError::Unreachable("fixture pipeline is down".into()));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let text = req.inputs["prompt"].as_str().unwrap_or("fixture pipeline").to_string();
        let svg = super::comfy::render_svg(&json!({ "1": { "inputs": { "text": text } } }), &id);
        let caption = format!("{}: {text}\n", req.pipeline_id).into_bytes();
        let files = vec![artifact("render", "image/svg+xml", &svg), artifact("caption", "text/plain", &caption)];
        let manifest: Vec<contracts::ArtifactRef> = files.iter().map(|(a, _)| a.clone()).collect();
        lock(&self.0.runs).insert(id.clone(), (PipelineStatus::Pending, None));
        let run = PipelineRun { external_id: id.clone(), raw: json!({ "fixture": true, "pipeline_id": req.pipeline_id }) };
        // Hold the handle in flight so a test can cancel while it is unrecorded.
        if self.0.windmill.pause_submit.swap(false, std::sync::atomic::Ordering::SeqCst) {
            self.0.windmill.submit_started.notify_one();
            self.0.windmill.submit_release.notified().await;
        }
        // Manual mode: the test posts the manifest and uploads itself.
        if self.0.windmill.manual_pipeline.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(run);
        }
        let me = self.clone();
        let req = req.clone();
        let task_id = id.clone();
        let task = tokio::spawn(async move {
            let mut status = PipelineStatus::Succeeded { artifacts: manifest.clone() };
            for (entry, bytes) in &files {
                if let Err(e) = me.upload(&req.artifact_url, &link.token, entry, bytes).await {
                    status = PipelineStatus::Failed { reason: e };
                    break;
                }
            }
            let receipt = ReceiptIn {
                kind: AttemptKind::PipelineRun,
                external_id: task_id.clone(),
                parent_external_id: None,
                engine_instance_id: None,
                status: Some(match &status {
                    PipelineStatus::Succeeded { .. } => contracts::JobStatus::Succeeded,
                    _ => contracts::JobStatus::Failed,
                }),
                artifacts: match &status {
                    PipelineStatus::Succeeded { artifacts } => artifacts.clone(),
                    _ => Vec::new(),
                },
                raw: json!({ "fixture": true }),
            };
            let path = req.receipt_url.split("/v1").nth(1).unwrap_or_default().to_string();
            let client = contracts::client::HubClient::new(&link.url, &link.token);
            for attempt in 0..3 {
                match client.post::<_, Value>(&path, &receipt).await {
                    Ok(_) => break,
                    Err(_) => tokio::time::sleep(std::time::Duration::from_millis(100 << attempt)).await,
                }
            }
            me.set(&task_id, status);
        });
        if let Some(r) = lock(&self.0.runs).get_mut(&id) {
            r.1 = Some(task.abort_handle());
        }
        Ok(run)
    }

    async fn status(&self, run: &PipelineRun) -> PortResult<PipelineStatus> {
        if self.0.windmill.bad_manifest.swap(false, std::sync::atomic::Ordering::SeqCst) {
            // A completed run whose terminal output the hub cannot accept.
            return Ok(PipelineStatus::Succeeded {
                artifacts: vec![contracts::ArtifactRef {
                    source: "render".into(),
                    sha256: "not-a-digest".into(),
                    mime: "image/svg+xml".into(),
                    size: 7,
                }],
            });
        }
        // Never hold the run lock across the wait: a parked read must not block
        // `cancel` or the fixture's own bookkeeping.
        let status = {
            let mut runs = lock(&self.0.runs);
            let r = runs.get_mut(&run.external_id).ok_or(PortError::NotFound)?;
            if self.0.windmill.defer_next_status.swap(false, std::sync::atomic::Ordering::SeqCst) {
                r.0 = PipelineStatus::Deferred { reason: "GPU busy (fixture)".into() };
            }
            r.0.clone()
        };
        if self.0.windmill.slow_status.swap(false, std::sync::atomic::Ordering::SeqCst) {
            self.0.windmill.status_started.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(status)
    }

    async fn cancel(&self, run: &PipelineRun) -> PortResult<CancelOutcome> {
        let mut runs = lock(&self.0.runs);
        let r = runs.get_mut(&run.external_id).ok_or(PortError::NotFound)?;
        match &r.0 {
            PipelineStatus::Succeeded { .. } | PipelineStatus::Failed { .. } | PipelineStatus::Cancelled => {
                Ok(CancelOutcome::AlreadyFinished)
            }
            _ => {
                if let Some(h) = r.1.take() {
                    h.abort();
                }
                r.0 = PipelineStatus::Cancelled;
                Ok(CancelOutcome::Interrupted)
            }
        }
    }
}
