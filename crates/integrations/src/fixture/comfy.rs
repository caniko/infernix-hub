//! Fixture ComfyUI: one GPU worker with a FIFO queue, per-step progress,
//! history, `/view`, targeted cancellation and history clearing.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use domain::ports::*;
use serde_json::Value;
use tokio::sync::broadcast;

use super::lock;

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

pub(super) fn render_svg(graph: &Value, prompt_id: &str) -> Vec<u8> {
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
