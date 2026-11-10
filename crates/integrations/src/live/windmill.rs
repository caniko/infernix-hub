//! Windmill CE adapter (1.839.0).
//!
//! Contract source: live `GET /api/openapi.yaml` (servers: `/api`), retrieved
//! 2026-11-09. Verified shapes:
//! * `POST /api/w/{workspace}/jobs/run/p/{path}` → `201 text/plain` job uuid
//!   (optionally `?scheduled_for=<RFC3339>`);
//! * `POST /api/w/{workspace}/jobs/run/h/{hash}` → same, used when the registry
//!   path is version-pinned as `path@hash`;
//! * `GET /api/w/{workspace}/jobs_u/get/{id}` → `CompletedJob | QueuedJob`;
//! * `POST /api/w/{workspace}/jobs_u/queue/cancel/{id}` → `200 text/plain`.
//!
//! Version pinning: when `GenerationJob.windmill_path` contains `@`, the part
//! after the last `@` is a script hash and the run goes through `run/h/{hash}`,
//! so a redeploy of the path cannot change the code a queued job executes.

use async_trait::async_trait;
use chrono::SecondsFormat;
use domain::ports::{GenerationJob, JobStatus, JobView, PortError, PortResult, PublicationTimer, Windmill};
use serde_json::{Value, json};

use super::{read_json, read_text, transport_error, trim_base};

/// Windmill connection settings. `Debug` is manual so the workspace token
/// cannot leak through `{:?}`.
#[derive(Clone)]
pub struct WindmillConfig {
    /// Origin of the Windmill server, e.g. `http://127.0.0.1:8040`.
    pub base_url: String,
    /// Workspace slug, e.g. `studio`.
    pub workspace: String,
    /// Workspace token (`Authorization: Bearer …`).
    pub token: String,
}

impl WindmillConfig {
    pub fn new(base_url: impl Into<String>, workspace: impl Into<String>, token: impl Into<String>) -> Self {
        Self { base_url: trim_base(&base_url.into()), workspace: workspace.into(), token: token.into() }
    }
}

impl std::fmt::Debug for WindmillConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindmillConfig")
            .field("base_url", &self.base_url)
            .field("workspace", &self.workspace)
            .field("token", &"<redacted>")
            .finish()
    }
}

pub struct WindmillClient {
    http: reqwest::Client,
    config: WindmillConfig,
}

impl WindmillClient {
    pub fn new(http: reqwest::Client, config: WindmillConfig) -> Self {
        Self { http, config }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/api/w/{}{}", self.config.base_url, self.config.workspace, path)
    }

    fn auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        rb.bearer_auth(&self.config.token)
    }

    /// Split `f/foo/bar@deadbeef` into (`f/foo/bar`, Some(`deadbeef`)).
    fn split_pinned_path(path: &str) -> (&str, Option<&str>) {
        match path.rsplit_once('@') {
            Some((path, hash)) if !path.is_empty() && !hash.is_empty() => (path, Some(hash)),
            _ => (path, None),
        }
    }

    async fn run(&self, path: &str, scheduled_for: Option<String>, body: &Value) -> PortResult<String> {
        let (path, hash) = Self::split_pinned_path(path);
        let path = path.trim_start_matches('/');
        let mut url = match hash {
            Some(hash) => self.url(&format!("/jobs/run/h/{hash}")),
            None => self.url(&format!("/jobs/run/p/{path}")),
        };
        if let Some(scheduled_for) = scheduled_for {
            url.push_str(if url.contains('?') { "&" } else { "?" });
            url.push_str("scheduled_for=");
            url.push_str(&scheduled_for);
        }
        let resp = self.auth(self.http.post(url)).json(body).send().await.map_err(|e| transport_error(e, true))?;
        let text = read_text(resp, false, true).await?;
        let id = text.trim_matches('"').trim().to_string();
        if id.is_empty() {
            return Err(PortError::Ambiguous("windmill accepted the run but returned no job id".into()));
        }
        Ok(id)
    }
}

/// Map a Windmill `Job` (discriminated on `type`) onto [`JobStatus`].
fn parse_job(id: &str, value: Value) -> PortResult<JobView> {
    let job_type = value.get("type").and_then(Value::as_str).unwrap_or_default();
    let status = match job_type {
        "CompletedJob" => {
            if value.get("canceled").and_then(Value::as_bool).unwrap_or(false) {
                JobStatus::Cancelled
            } else if value.get("success").and_then(Value::as_bool).unwrap_or(false) {
                JobStatus::Succeeded
            } else {
                JobStatus::Failed
            }
        }
        "QueuedJob" => {
            if value.get("running").and_then(Value::as_bool).unwrap_or(false) {
                JobStatus::Running
            } else {
                JobStatus::Queued
            }
        }
        other => {
            return Err(PortError::Unsupported(format!("windmill job type `{other}` is not mapped")));
        }
    };
    Ok(JobView { id: id.to_string(), status, raw: value })
}

#[async_trait]
impl Windmill for WindmillClient {
    async fn run_generation(&self, job: &GenerationJob) -> PortResult<String> {
        // Windmill maps body keys to `main`'s parameter names: `main(job: GenerationJob)`.
        self.run(&job.windmill_path, None, &json!({ "job": job })).await
    }

    async fn schedule_publication(&self, timer: &PublicationTimer) -> PortResult<String> {
        let scheduled_for = timer.due_at.to_rfc3339_opts(SecondsFormat::Secs, true);
        self.run("f/infernix_hub/publication_timer", Some(scheduled_for), &json!({ "timer": timer })).await
    }

    async fn job(&self, id: &str) -> PortResult<JobView> {
        let url = self.url(&format!("/jobs_u/get/{id}"));
        let resp = self
            .auth(self.http.get(url))
            .query(&[("no_logs", "true"), ("no_code", "true")])
            .send()
            .await
            .map_err(|e| transport_error(e, false))?;
        let value = read_json(resp, true, false).await?;
        parse_job(id, value)
    }

    async fn cancel_job(&self, id: &str) -> PortResult<()> {
        let url = self.url(&format!("/jobs_u/queue/cancel/{id}"));
        let resp = self
            .auth(self.http.post(url))
            .json(&json!({ "reason": "cancelled by infernix-hub" }))
            .send()
            .await
            .map_err(|e| transport_error(e, true))?;
        read_text(resp, false, true).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> WindmillConfig {
        WindmillConfig::new("http://127.0.0.1:8040/", "studio", "wm-secret-token")
    }

    #[test]
    fn debug_redacts_the_workspace_token() {
        let dbg = format!("{:?}", config());
        assert!(!dbg.contains("wm-secret-token"), "secret leaked: {dbg}");
        assert!(dbg.contains("<redacted>"));
        assert!(dbg.contains("studio"));
    }

    #[test]
    fn pinned_paths_split_on_the_last_at() {
        assert_eq!(
            WindmillClient::split_pinned_path("f/infernix_hub/comfy_generate@abc123"),
            ("f/infernix_hub/comfy_generate", Some("abc123"))
        );
        assert_eq!(WindmillClient::split_pinned_path("f/infernix_hub/comfy_generate"), ("f/infernix_hub/comfy_generate", None));
        assert_eq!(WindmillClient::split_pinned_path("@abc"), ("@abc", None));
    }

    #[test]
    fn completed_job_maps_success_cancel_and_failure() {
        let value: Value = serde_json::from_str(include_str!("testdata/windmill-job-completed.json")).unwrap();
        assert_eq!(parse_job("j1", value).unwrap().status, JobStatus::Succeeded);

        let mut canceled = serde_json::from_str::<Value>(include_str!("testdata/windmill-job-completed.json")).unwrap();
        canceled["canceled"] = json!(true);
        assert_eq!(parse_job("j1", canceled).unwrap().status, JobStatus::Cancelled);

        let mut failed = serde_json::from_str::<Value>(include_str!("testdata/windmill-job-completed.json")).unwrap();
        failed["success"] = json!(false);
        assert_eq!(parse_job("j1", failed).unwrap().status, JobStatus::Failed);
    }

    #[test]
    fn queued_job_maps_running_and_queued() {
        let mut value: Value = serde_json::from_str(include_str!("testdata/windmill-job-queued.json")).unwrap();
        assert_eq!(parse_job("j2", value.clone()).unwrap().status, JobStatus::Queued);
        value["running"] = json!(true);
        assert_eq!(parse_job("j2", value).unwrap().status, JobStatus::Running);
    }

    #[test]
    fn unknown_job_type_is_unsupported() {
        let err = parse_job("j3", json!({"type": "MysteryJob"})).unwrap_err();
        assert!(matches!(err, PortError::Unsupported(_)));
    }
}
