//! In-process fixtures for every external system. They reproduce the
//! contract behaviour the hub depends on (ids, async completion, history,
//! targeted cancellation, ambiguous timeouts) so the full journey runs with no
//! credentials and no GPU. They prove hub logic only, never a live integration.

use std::sync::{Mutex, MutexGuard};

mod assistant;
mod comfy;
mod paperclip;
mod publisher;
mod windmill;

pub use assistant::FixtureAssistant;
pub use comfy::FixtureComfy;
pub use paperclip::FixturePaperclip;
pub use publisher::{FixturePublisher, PublishMode};
pub use windmill::{FixturePipeline, FixtureWindmill, WindmillState};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ── Shared-GPU peer ─────────────────────────────────────────────────────────

/// Simulated llama-swap: `loaded` says whether a model holds the GPU.
#[derive(Default)]
pub struct FixtureGpuPeer {
    pub loaded: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl domain::ports::GpuPeer for FixtureGpuPeer {
    async fn busy(&self, _running_url: &str) -> domain::ports::PortResult<bool> {
        Ok(self.loaded.load(std::sync::atomic::Ordering::SeqCst))
    }
}
