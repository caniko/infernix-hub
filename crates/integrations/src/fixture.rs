//! In-process fixtures for every external system. They reproduce the
//! contract behaviour the hub depends on (ids, async completion, history,
//! targeted cancellation, ambiguous timeouts) so the full journey runs with no
//! credentials and no GPU. They prove hub logic only, never a live integration.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use chrono::Utc;
use contracts::{AttemptKind, CanonBody, DispatchIn, ReceiptIn};
use domain::ports::*;
use serde_json::{Value, json};
use tokio::sync::broadcast;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ── Paperclip ───────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct FixturePaperclip {
    issues: Mutex<HashMap<String, ForeignRef>>,
    approvals: Mutex<HashMap<String, ApprovalView>>,
    /// When set, every call fails as unreachable (fail-closed tests).
    pub unreachable: std::sync::atomic::AtomicBool,
}

impl FixturePaperclip {
    fn check(&self) -> PortResult<()> {
        if self.unreachable.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(PortError::Unreachable("fixture paperclip offline".into()));
        }
        Ok(())
    }

    /// Board decision in the fixture Paperclip. `user` is the deciding human.
    pub fn decide(&self, approval_id: &str, approve: bool, user: &str) -> PortResult<ApprovalView> {
        let mut a = lock(&self.approvals);
        let v = a.get_mut(approval_id).ok_or(PortError::NotFound)?;
        v.status = if approve { ApprovalStatus::Approved } else { ApprovalStatus::Rejected };
        v.decided_by_user_id = Some(user.to_string());
        v.decided_at = Some(Utc::now());
        v.raw["status"] = json!(v.status);
        Ok(v.clone())
    }

    pub fn approvals(&self) -> Vec<ApprovalView> {
        lock(&self.approvals).values().cloned().collect()
    }
}

#[async_trait]
impl Paperclip for FixturePaperclip {
    async fn create_issue(&self, req: &IssueRequest) -> PortResult<ForeignRef> {
        self.check()?;
        let mut issues = lock(&self.issues);
        let n = issues.len() + 1;
        Ok(issues
            .entry(req.origin_key.clone())
            .or_insert_with(|| ForeignRef { id: format!("fixture-issue-{n}"), url: Some(format!("fixture://paperclip/issues/{n}")) })
            .clone())
    }

    async fn create_approval(&self, req: &ApprovalRequest) -> PortResult<ForeignRef> {
        self.check()?;
        let mut a = lock(&self.approvals);
        if let Some(v) = a.values().find(|v| v.release_revision_id.as_deref() == Some(&req.release_revision_id)) {
            return Ok(ForeignRef { id: v.id.clone(), url: None });
        }
        let id = format!("fixture-approval-{}", a.len() + 1);
        a.insert(
            id.clone(),
            ApprovalView {
                id: id.clone(),
                status: ApprovalStatus::Pending,
                decided_by_user_id: None,
                decided_at: None,
                payload_hash: Some(req.payload_hash.clone()),
                release_revision_id: Some(req.release_revision_id.clone()),
                raw: json!({ "fixture": true, "type": "general", "title": req.title, "payload": { "payloadHash": req.payload_hash } }),
            },
        );
        Ok(ForeignRef { id: id.clone(), url: Some(format!("fixture://paperclip/approvals/{id}")) })
    }

    async fn get_approval(&self, id: &str) -> PortResult<ApprovalView> {
        self.check()?;
        lock(&self.approvals).get(id).cloned().ok_or(PortError::NotFound)
    }
}

// ── ComfyUI ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct FixturePrompt {
    id: String,
    client_id: String,
    graph: Value,
    steps_done: u64,
}

#[derive(Default)]
struct ComfyState {
    queue: VecDeque<FixturePrompt>,
    running: Option<FixturePrompt>,
    history: HashMap<String, PromptOutcome>,
    files: HashMap<String, Vec<u8>>,
}

/// Simulated ComfyUI with one GPU worker: FIFO queue, per-step progress,
/// history, `/view`, targeted cancellation, and history clearing.
pub struct FixtureComfy {
    state: Mutex<ComfyState>,
    events: broadcast::Sender<(String, ProgressEvent)>,
    pub steps: u64,
}

impl Default for FixtureComfy {
    fn default() -> Self {
        Self { state: Mutex::default(), events: broadcast::channel(1024).0, steps: 3 }
    }
}

impl FixtureComfy {
    /// `POST /prompt` with a caller-chosen prompt id.
    pub fn submit(&self, prompt_id: &str, client_id: &str, graph: Value) -> String {
        let mut s = lock(&self.state);
        let known = s.history.contains_key(prompt_id)
            || s.queue.iter().any(|p| p.id == prompt_id)
            || s.running.as_ref().is_some_and(|p| p.id == prompt_id);
        if !known {
            s.queue.push_back(FixturePrompt { id: prompt_id.into(), client_id: client_id.into(), graph, steps_done: 0 });
        }
        prompt_id.into()
    }

    fn emit(&self, client: &str, ev: ProgressEvent) {
        let _ = self.events.send((client.to_string(), ev));
    }

    /// Advance the single worker by one step.
    pub fn tick(&self) {
        let mut out = vec![];
        {
            let mut s = lock(&self.state);
            if s.running.is_none()
                && let Some(p) = s.queue.pop_front()
            {
                out.push((p.client_id.clone(), ProgressEvent::Started { prompt_id: p.id.clone() }));
                s.running = Some(p);
            }
            let steps = self.steps;
            let finished = match s.running.as_mut() {
                Some(p) => {
                    p.steps_done += 1;
                    out.push((
                        p.client_id.clone(),
                        ProgressEvent::Progress { prompt_id: p.id.clone(), node: Some("3".into()), value: p.steps_done, max: steps },
                    ));
                    if p.steps_done >= steps {
                        // A node output event is not workflow success.
                        out.push((p.client_id.clone(), ProgressEvent::NodeExecuted { prompt_id: p.id.clone(), node: "9".into() }));
                        Some(p.clone())
                    } else {
                        None
                    }
                }
                None => None,
            };
            if let Some(p) = finished {
                s.running = None;
                let filename = format!("{}_00001_.svg", &p.id[..8.min(p.id.len())]);
                s.files.insert(filename.clone(), render_svg(&p.graph, &p.id));
                s.history.insert(
                    p.id.clone(),
                    PromptOutcome::Succeeded {
                        outputs: vec![OutputFile { node_id: "9".into(), filename, subfolder: "".into(), kind: "output".into() }],
                    },
                );
                out.push((p.client_id.clone(), ProgressEvent::Finished { prompt_id: p.id.clone() }));
            }
        }
        for (c, e) in out {
            self.emit(&c, e);
        }
    }

    pub fn spawn_runner(self: &Arc<Self>, every: std::time::Duration) -> tokio::task::JoinHandle<()> {
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                me.tick();
            }
        })
    }

    /// Simulate engine history retention (`POST /history {"clear": true}`).
    pub fn clear_history(&self) {
        let mut s = lock(&self.state);
        s.history.clear();
        s.files.clear();
    }

    pub fn running(&self) -> Option<String> {
        lock(&self.state).running.as_ref().map(|p| p.id.clone())
    }

    pub fn queued(&self) -> Vec<String> {
        lock(&self.state).queue.iter().map(|p| p.id.clone()).collect()
    }
}

fn render_svg(graph: &Value, prompt_id: &str) -> Vec<u8> {
    let text = graph
        .as_object()
        .and_then(|g| g.values().find_map(|n| n["inputs"]["text"].as_str()))
        .unwrap_or("fixture")
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == ' ')
        .take(60)
        .collect::<String>();
    let hue = prompt_id.bytes().fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32)) % 360;
    format!(
        "<svg xmlns='http://www.w3.org/2000/svg' width='512' height='512' viewBox='0 0 512 512'>\
         <rect width='512' height='512' fill='hsl({hue},45%,22%)'/>\
         <circle cx='256' cy='220' r='120' fill='hsl({},70%,55%)'/>\
         <text x='256' y='430' font-family='monospace' font-size='20' fill='#ebdbb2' text-anchor='middle'>{text}</text>\
         <text x='256' y='470' font-family='monospace' font-size='14' fill='#bdae93' text-anchor='middle'>FIXTURE · {}</text></svg>",
        (hue + 40) % 360,
        &prompt_id[..8.min(prompt_id.len())]
    )
    .into_bytes()
}

#[async_trait]
impl Comfy for FixtureComfy {
    async fn load(&self, _base_url: &str) -> PortResult<EngineLoad> {
        let s = lock(&self.state);
        Ok(EngineLoad {
            running: s.running.iter().map(|p| p.id.clone()).collect(),
            pending: s.queue.iter().map(|p| p.id.clone()).collect(),
        })
    }

    async fn subscribe(&self, _base_url: &str, client_id: &str, tx: tokio::sync::mpsc::Sender<ProgressEvent>) -> PortResult<()> {
        let mut rx = self.events.subscribe();
        loop {
            match rx.recv().await {
                Ok((c, ev)) if c == client_id => {
                    if tx.send(ev).await.is_err() {
                        return Ok(());
                    }
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => return Ok(()),
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            }
        }
    }

    async fn outcome(&self, _base_url: &str, prompt_id: &str) -> PortResult<PromptOutcome> {
        let s = lock(&self.state);
        if let Some(h) = s.history.get(prompt_id) {
            return Ok(h.clone());
        }
        if s.running.as_ref().is_some_and(|p| p.id == prompt_id) {
            return Ok(PromptOutcome::Pending { queue_position: None, running: true });
        }
        if let Some(pos) = s.queue.iter().position(|p| p.id == prompt_id) {
            return Ok(PromptOutcome::Pending { queue_position: Some(pos), running: false });
        }
        Ok(PromptOutcome::Unknown)
    }

    async fn fetch(&self, _base_url: &str, file: &OutputFile) -> PortResult<Vec<u8>> {
        lock(&self.state).files.get(&file.filename).cloned().ok_or(PortError::NotFound)
    }

    async fn cancel(&self, _base_url: &str, prompt_id: &str) -> PortResult<CancelOutcome> {
        let mut s = lock(&self.state);
        if let Some(pos) = s.queue.iter().position(|p| p.id == prompt_id) {
            s.queue.remove(pos);
            return Ok(CancelOutcome::Dequeued);
        }
        if s.running.as_ref().is_some_and(|p| p.id == prompt_id) {
            let p = s.running.take().expect("checked");
            s.history.insert(p.id.clone(), PromptOutcome::Failed { message: "interrupted".into() });
            drop(s);
            self.emit(&p.client_id, ProgressEvent::Interrupted { prompt_id: p.id });
            return Ok(CancelOutcome::Interrupted);
        }
        if s.history.contains_key(prompt_id) {
            return Ok(CancelOutcome::AlreadyFinished);
        }
        Err(PortError::NotFound)
    }
}

// ── Windmill ────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct HubLink {
    url: String,
    token: String,
}

/// Runs the two registered procedures in-process with the same contract as
/// `registry/windmill/*.ts`: receipts and dispatch go over the hub's HTTP API.
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

// ── Publishing transport ────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishMode {
    Publish,
    /// Accepted now, confirmed published on the next lookup.
    AcceptThenPublish,
    Reject,
    /// Records the post but the response is lost.
    TimeoutAfterSend,
    Unreachable,
}

#[derive(Default)]
pub struct FixturePublisher {
    posts: Mutex<HashMap<String, ProviderPost>>,
    replies: Mutex<HashMap<String, ProviderPost>>,
    modes: Mutex<HashMap<String, PublishMode>>,
    submissions: Mutex<Vec<PublishRequest>>,
    reply_submissions: Mutex<Vec<ReplyRequest>>,
}

impl FixturePublisher {
    pub fn set_mode(&self, account: &str, mode: PublishMode) {
        lock(&self.modes).insert(account.into(), mode);
    }

    fn mode(&self, account: &str) -> PublishMode {
        lock(&self.modes).get(account).copied().unwrap_or(PublishMode::Publish)
    }

    pub fn submissions(&self) -> Vec<PublishRequest> {
        lock(&self.submissions).clone()
    }

    pub fn reply_submissions(&self) -> Vec<ReplyRequest> {
        lock(&self.reply_submissions).clone()
    }
}

fn post(id: String, state: ProviderState) -> ProviderPost {
    ProviderPost {
        url: Some(format!("fixture://platform/posts/{id}")),
        raw: json!({ "fixture": true, "id": id, "state": state }),
        provider_id: id,
        state,
    }
}

#[async_trait]
impl Publisher for FixturePublisher {
    async fn submit(&self, req: &PublishRequest) -> PortResult<ProviderPost> {
        let mode = self.mode(&req.account_id);
        if mode == PublishMode::Unreachable {
            return Err(PortError::Unreachable("fixture transport offline".into()));
        }
        lock(&self.submissions).push(req.clone());
        let id = format!("fixture-post-{}", req.idempotency_key);
        match mode {
            PublishMode::Reject => Err(PortError::Rejected { status: 422, message: "fixture platform rejected the post".into() }),
            PublishMode::TimeoutAfterSend => {
                lock(&self.posts).insert(req.idempotency_key.clone(), post(id, ProviderState::Published));
                Err(PortError::Ambiguous("fixture timeout after send".into()))
            }
            PublishMode::AcceptThenPublish => {
                let p = post(id, ProviderState::Accepted);
                lock(&self.posts).insert(req.idempotency_key.clone(), p.clone());
                Ok(p)
            }
            _ => {
                let p = post(id, ProviderState::Published);
                lock(&self.posts).insert(req.idempotency_key.clone(), p.clone());
                Ok(p)
            }
        }
    }

    async fn lookup(&self, _account_id: &str, key: &str) -> PortResult<Option<ProviderPost>> {
        let mut posts = lock(&self.posts);
        Ok(posts.get_mut(key).map(|p| {
            if p.state == ProviderState::Accepted {
                p.state = ProviderState::Published;
            }
            p.clone()
        }))
    }

    async fn reply(&self, req: &ReplyRequest) -> PortResult<ProviderPost> {
        let mode = self.mode(&req.account_id);
        if mode == PublishMode::Unreachable {
            return Err(PortError::Unreachable("fixture transport offline".into()));
        }
        lock(&self.reply_submissions).push(req.clone());
        let p = post(format!("fixture-reply-{}", req.idempotency_key), ProviderState::Published);
        lock(&self.replies).insert(req.idempotency_key.clone(), p.clone());
        if mode == PublishMode::TimeoutAfterSend {
            return Err(PortError::Ambiguous("fixture timeout after send".into()));
        }
        Ok(p)
    }

    async fn lookup_reply(&self, _account_id: &str, key: &str) -> PortResult<Option<ProviderPost>> {
        Ok(lock(&self.replies).get(key).cloned())
    }
}

// ── Assistant ───────────────────────────────────────────────────────────────

/// Deterministic creative assistant. A message containing `rule:` yields a
/// canon suggestion (which the hub records as a proposal only).
pub struct FixtureAssistant;

#[async_trait]
impl Assistant for FixtureAssistant {
    async fn reply(&self, context: &Value, turns: &[AssistantTurn]) -> PortResult<AssistantReply> {
        let last = turns.iter().rev().find(|t| t.role == "operator").map(|t| t.body.clone()).unwrap_or_default();
        let persona = context["persona"]["name"].as_str().unwrap_or("the persona");
        let canon_suggestion = last.split_once("rule:").map(|(_, rule)| {
            let mut body: CanonBody = serde_json::from_value(context["persona"]["canon"]["body"].clone()).unwrap_or_default();
            if body.voice.is_empty() {
                body.voice = format!("{persona} voice");
            }
            body.rules.push(rule.trim().to_string());
            body
        });
        let body = match &canon_suggestion {
            Some(_) => format!("[fixture] I drafted a canon proposal for {persona}. It stays pending until you review it."),
            None => format!(
                "[fixture] For {persona}: consider leading with the strongest visual, then one concrete detail. You said: \"{last}\""
            ),
        };
        Ok(AssistantReply { body, canon_suggestion, model: "fixture-assistant".into() })
    }
}

// ── Shared-GPU peer ─────────────────────────────────────────────────────────

/// Simulated llama-swap: `loaded` says whether a model holds the GPU.
#[derive(Default)]
pub struct FixtureGpuPeer {
    pub loaded: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl GpuPeer for FixtureGpuPeer {
    async fn busy(&self, _running_url: &str) -> PortResult<bool> {
        Ok(self.loaded.load(std::sync::atomic::Ordering::SeqCst))
    }
}
