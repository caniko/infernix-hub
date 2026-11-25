use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    env, fs,
    path::{Path, PathBuf},
    sync::{Arc, Weak},
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use contracts::providers::*;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::config::{Config, origin};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Lane {
    Claude,
    Codex,
    Google,
    Go,
}

impl Lane {
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "claude" | "anthropic" => Self::Claude,
            "codex" | "openai" | "openai_compat" => Self::Codex,
            "antigravity" | "google" | "gemini" => Self::Google,
            "opencode-go" | "opencode" => Self::Go,
            _ => return None,
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Google => "antigravity",
            Self::Go => "opencode-go",
        }
    }
    pub fn family(self) -> &'static str {
        match self {
            Self::Codex | Self::Go => "opencode",
            _ => self.name(),
        }
    }
}

pub(crate) fn window_applies_to_model(w: &QuotaWindow, model: &str) -> bool {
    if w.kind == QuotaWindowKind::Spend || w.unit == QuotaUnit::Credits {
        return false;
    }
    let Some(scope) = &w.scope_model else { return true };
    if model.is_empty() {
        return false;
    }
    let (model, key, scope) = (model.to_ascii_lowercase(), w.window_key.to_ascii_lowercase(), scope.to_ascii_lowercase());
    let gemini = model.starts_with("gemini-");
    let argon = model.contains("argon");
    let claude = model.starts_with("claude-");
    let gpt = model.starts_with("gpt-");
    if key.starts_with("gemini-argon") {
        return gemini && argon;
    }
    if key.starts_with("gemini-") {
        return gemini && !argon;
    }
    if key.starts_with("3p-") {
        return claude || gpt;
    }
    match key.as_str() {
        "seven_day_opus" => return model.contains("opus"),
        "seven_day_sonnet" => return model.contains("sonnet"),
        "seven_day_haiku" => return model.contains("haiku"),
        "seven_day_oauth_apps" | "seven_day_cowork" => return model.contains(&scope),
        _ => {}
    }
    match scope.as_str() {
        _ if scope.contains("argon") => gemini && argon,
        "gemini" => gemini && !argon,
        "claude & gpt (shared)" | "claude-gpt" => claude || gpt,
        "claude" => claude,
        "gpt" => gpt,
        "opus" | "claude opus" => model.contains("opus"),
        "sonnet" | "claude sonnet" => model.contains("sonnet"),
        "haiku" | "claude haiku" => model.contains("haiku"),
        _ => model == scope || model.starts_with(&format!("{scope}-")) || (!scope.contains(' ') && model.contains(&scope)),
    }
}

pub(crate) fn model_cooldown_scope(lane: Lane, model: &str) -> Option<String> {
    if lane != Lane::Google || model.is_empty() {
        return None;
    }
    let model = model.to_ascii_lowercase();
    Some(if model.starts_with("gemini-") {
        if model.contains("argon") { "gemini-argon".into() } else { "gemini".into() }
    } else if model.starts_with("claude-") || model.starts_with("gpt-") {
        "3p".into()
    } else {
        "other".into()
    })
}

pub(crate) fn enforces_quota(w: &QuotaWindow) -> bool {
    w.kind != QuotaWindowKind::Spend
        && w.unit != QuotaUnit::Credits
        && (w.status == QuotaStatus::Rejected || w.used_percent.is_some_and(|n| n >= 100.0))
}

pub(crate) fn quota_window_active(lane: Lane, w: &QuotaWindow, now: DateTime<Utc>) -> bool {
    w.resets_at
        .map_or_else(|| lane != Lane::Google || w.observed_at.is_some_and(|at| now - at < chrono::TimeDelta::hours(1)), |at| at > now)
}

pub(crate) const MAX_COOLDOWN_SECS: i64 = 604_800;

pub(crate) fn bounded_identifier(model: &str) -> bool {
    !model.is_empty() && model.len() <= 192 && model.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-:/".contains(&b))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cooldown {
    pub until: DateTime<Utc>,
    pub verified_reset: bool,
}

impl Cooldown {
    pub(crate) fn merge(self, other: Self) -> Self {
        // A local or clamped blocker wins ties: it cannot vouch for a reset.
        if (other.until, !other.verified_reset) > (self.until, !self.verified_reset) { other } else { self }
    }
}

pub(crate) struct QuotaReader(Arc<()>);

#[derive(Clone)]
struct PendingQuota {
    reader: Weak<()>,
    scope: Option<String>,
    provisional: Option<Cooldown>,
    unverified_until: Option<DateTime<Utc>>,
}

impl PendingQuota {
    fn blocks_verification(&self, now: DateTime<Utc>) -> bool {
        self.reader.strong_count() != 0 || self.unverified_until.or(self.provisional.map(|c| c.until)).is_some_and(|until| until > now)
    }
}

#[derive(Clone)]
pub struct Account {
    pub id: String,
    pub name: String,
    pub auth_index: String,
    pub lane: Lane,
    pub provider: &'static str,
    pub label: String,
    pub email: Option<String>,
    pub disabled: bool,
    pub priority: i32,
    pub weight: u32,
    pub note: Option<String>,
    pub plan: Option<String>,
    pub multiplier: Option<f64>,
    pub access: String,
    pub refresh: Option<String>,
    // In-memory only: labels on disk can still contain retired credentials.
    // Keep safe labels keyed by the original label digest for the life of this account.
    redacted_labels: HashMap<[u8; 32], String>,
    pub expires: Option<DateTime<Utc>>,
    pub chatgpt_id: Option<String>,
    pub chatgpt_user: Option<String>,
    pub organization: Option<String>,
    pub google_client: Option<String>,
    pub google_secret: Option<String>,
    provenance_enrollment: Option<String>,
    provenance_project: Option<String>,
    pub project: Option<String>,
    pub api_key: bool,
    pub(crate) codex_oauth_explicit: bool,
    pub base: Option<reqwest::Url>,
    pub local: bool,
    pub models: Vec<String>,
    pub cooldown: Option<Cooldown>,
    pub scoped_cooldowns: BTreeMap<String, Cooldown>,
    pending_quota: Vec<PendingQuota>,
    pub last_error: Option<AccountError>,
    pub quota_probe_policy_error: Option<AccountError>,
    pub error_generation: u64,
    pub source_fingerprint: Option<String>,
    pub windows: Vec<QuotaWindow>,
    pub google_fallback_keys: BTreeSet<String>,
    pub grants: Vec<ResetGrant>,
    pub next_grant: Option<String>,
    pub observed: Option<DateTime<Utc>>,
    pub last_used: Option<DateTime<Utc>>,
    pub probe_failed: bool,
    pub probe_generation: u64,
    provenance_token_fp: Option<[u8; 32]>,
    pub success: u64,
    pub failure: u64,
}

impl Account {
    fn is_codex_oauth(&self) -> bool {
        self.lane == Lane::Codex
            && !self.api_key
            && !self.access.is_empty()
            && !self.access.starts_with("sk-")
            && (self.codex_oauth_explicit
                || self.refresh.as_deref().is_some_and(|token| {
                    !token.is_empty()
                        && token.len() <= 32_768
                        && token != "spillway-pool"
                        && !token.starts_with("sk-")
                        && token.bytes().all(|b| b.is_ascii_graphic())
                }))
    }

    fn is_google_oauth(&self) -> bool {
        self.lane == Lane::Google
            && !self.api_key
            && !self.access.starts_with("AIza")
            && (self.refresh.as_deref().is_some_and(|token| {
                !token.is_empty() && token.len() <= 32_768 && !token.starts_with("AIza") && token.bytes().all(|b| b.is_ascii_graphic())
            }) || (!self.access.is_empty() && self.access.starts_with("ya29.")))
    }

    pub(crate) fn current_secrets(&self) -> [&str; 3] {
        [&self.access, self.refresh.as_deref().unwrap_or_default(), self.google_secret.as_deref().unwrap_or_default()]
    }

    pub(crate) fn known_safe_label(&self, raw: &str) -> Option<&str> {
        self.redacted_labels.get(&<[u8; 32]>::from(Sha256::digest(raw))).map(String::as_str)
    }

    fn scrub_label(&mut self, previous_secrets: &[&str]) {
        let hash = Sha256::digest(&self.label).into();
        let mut redacted_labels = std::mem::take(&mut self.redacted_labels);
        let label = redacted_labels.get(&hash).unwrap_or(&self.label);
        let secrets = self.current_secrets().into_iter().chain(previous_secrets.iter().copied()).collect::<Vec<_>>();
        let safe = crate::error_classify::scrub_label(label, &secrets);
        // A newly known credential may also appear in an inactive saved label.
        for label in redacted_labels.values_mut() {
            if secrets.iter().any(|secret| !secret.is_empty() && label.contains(secret)) {
                *label = crate::error_classify::scrub_label(label, &secrets);
            }
        }
        if safe != self.label {
            redacted_labels.insert(hash, safe.clone());
        }
        self.redacted_labels = redacted_labels;
        self.label = safe;
    }

    pub(crate) fn replace_access(&mut self, access: String) {
        let old = std::mem::replace(&mut self.access, access);
        if old != self.access {
            self.quota_probe_policy_error = None;
        }
        self.scrub_label(&[&old]);
    }

    pub(crate) fn replace_refresh(&mut self, refresh: Option<String>) {
        let old = std::mem::replace(&mut self.refresh, refresh);
        self.scrub_label(&[old.as_deref().unwrap_or_default()]);
    }

    pub(crate) fn needs_google_metadata(&self) -> bool {
        self.lane == Lane::Google && !self.disabled && !self.api_key && (self.email.is_none() || self.project.is_none())
    }

    pub(crate) fn same_credential(&self, other: &Self) -> bool {
        self.lane == other.lane
            && self.api_key == other.api_key
            && (self.lane != Lane::Codex || (self.chatgpt_id == other.chatgpt_id && self.chatgpt_user == other.chatgpt_user))
            && (self.lane != Lane::Google || (self.project == other.project && self.base == other.base))
            && self.compatible_refresh(other)
            && ((!self.access.is_empty() && self.access == other.access) || (self.refresh.is_some() && self.refresh == other.refresh))
    }

    fn compatible_refresh(&self, other: &Self) -> bool {
        self.refresh.is_none() || other.refresh.is_none() || self.refresh == other.refresh
    }

    fn shares_discovery_token(&self, other: &Self) -> bool {
        self.lane == other.lane
            && ((!self.access.is_empty() && self.access == other.access) || (self.refresh.is_some() && self.refresh == other.refresh))
    }

    fn shares_credential_fingerprint(&self, other: &Self) -> bool {
        if self.lane != other.lane {
            return false;
        }
        let fps = self.credential_fingerprints();
        other.credential_fingerprints().iter().any(|fp| fps.contains(fp))
    }

    fn credential_fingerprints(&self) -> Vec<[u8; 32]> {
        let mut fps = Vec::with_capacity(3);
        if !self.access.is_empty() {
            fps.push(Sha256::digest(&self.access).into());
        }
        if let Some(fp) = self.provenance_token_fp
            && !fps.contains(&fp)
        {
            fps.push(fp);
        }
        if let Some(refresh) = self.refresh.as_deref().filter(|r| !r.is_empty()) {
            let fp: [u8; 32] = Sha256::digest(refresh).into();
            if !fps.contains(&fp) {
                fps.push(fp);
            }
        }
        fps
    }

    fn discard_discovered_access(&mut self) {
        // Remember the disk token without making it available for routing.
        self.provenance_token_fp = Some(Sha256::digest(&self.access).into());
        self.replace_access(String::new());
        self.expires = None;
    }

    pub(crate) fn set_google_project(&mut self, project: Option<String>) -> bool {
        if self.project == project {
            return false;
        }
        self.project = project;
        self.provenance_project = None;
        self.clear_google_route_evidence();
        true
    }

    fn clear_google_route_evidence(&mut self) {
        self.cooldown = None;
        self.scoped_cooldowns.clear();
        self.pending_quota.clear();
        self.windows.clear();
        self.google_fallback_keys.clear();
        self.observed = None;
        self.probe_failed = false;
        self.probe_generation = self.probe_generation.saturating_add(1);
        // Inference and probe errors describe the old route; OAuth errors describe the credential.
        if self.last_error.as_ref().is_some_and(|err| err.source != AccountErrorSource::OAuthRefresh) {
            self.clear_account_error();
        }
        self.error_generation = self.error_generation.saturating_add(1);
    }

    pub(crate) fn same_secret(&self, other: &Self) -> bool {
        self.same_credential(other)
            && self.access == other.access
            && self.refresh == other.refresh
            && self.google_client == other.google_client
            && self.google_secret == other.google_secret
    }

    pub fn record_account_error(&mut self, mut err: AccountError, now: DateTime<Utc>, backoff: Option<Duration>) -> bool {
        if err.source != AccountErrorSource::QuotaProbe {
            self.error_generation = self.error_generation.saturating_add(1);
        }
        let previous = self.last_error.as_ref();
        // Blocking kinds remain quarantined until recovery, even after their retry backoff.
        if !err.kind.is_blocking() && previous.is_some_and(|old| old.kind.is_blocking()) {
            return false;
        }
        let same_kind = previous.is_some_and(|old| old.kind == err.kind);
        err.consecutive_count = previous.filter(|_| same_kind).map_or(1, |old| old.consecutive_count.saturating_add(1));
        let was_quarantined = self.quarantined(now);
        let backoff = backoff
            .or_else(|| (err.kind == AccountErrorKind::UpstreamUnavailable && err.consecutive_count >= 3).then(|| Duration::from_secs(30)));
        err.backoff_until = backoff
            .and_then(|d| now.checked_add_signed(chrono::TimeDelta::from_std(d).ok()?))
            .max(previous.filter(|_| same_kind).and_then(|old| old.backoff_until));
        let changed = !previous.is_some_and(|old| {
            old.kind == err.kind
                && old.provider_code == err.provider_code
                && old.http_status == err.http_status
                && old.message == err.message
                && old.is_notification_worthy() == err.is_notification_worthy()
        }) || was_quarantined != err.blocks_routing(now);
        self.last_error = Some(err);
        changed
    }

    pub fn clear_account_error(&mut self) -> bool {
        self.last_error.take().is_some()
    }

    pub(crate) fn record_error_once(&mut self, error: &AccountError) -> bool {
        // A failed OAuth refresh is recorded at the endpoint and returned to
        // the inference or quota caller. Count the shared failure only once.
        if self.last_error.as_ref().is_some_and(|old| old.occurred_at >= error.occurred_at) {
            return false;
        }
        self.record_account_error(error.clone(), Utc::now(), crate::error_classify::blocking_backoff(error.kind))
    }

    pub(crate) fn quarantined(&self, now: DateTime<Utc>) -> bool {
        self.active_quarantine(now).is_some()
    }

    fn active_quarantine(&self, now: DateTime<Utc>) -> Option<&AccountError> {
        self.last_error.as_ref().filter(|error| error.blocks_routing(now))
    }

    pub(crate) fn has_account_error(&self) -> bool {
        self.reported_error().map_or(self.probe_failed, |err| err.blocks_routing(Utc::now()) || err.kind == AccountErrorKind::ProbeFailed)
    }

    pub(crate) fn reported_error(&self) -> Option<&AccountError> {
        self.last_error.as_ref().or(self.quota_probe_policy_error.as_ref())
    }

    pub fn eligible(&self, now: DateTime<Utc>, model: &str) -> bool {
        self.supports(model) && !self.quarantined(now) && self.quota_limited_until(now, model).is_none()
    }

    fn supports(&self, model: &str) -> bool {
        self.support_exclusion(model).is_none()
    }

    fn support_exclusion(&self, model: &str) -> Option<&'static str> {
        if self.disabled {
            Some("disabled")
        } else if self.weight == 0 {
            Some("zero_weight")
        } else if !model.is_empty() && !self.models.is_empty() && !self.models.iter().any(|id| id == model) {
            Some("unsupported_model")
        } else {
            None
        }
    }

    pub fn cooldown_until(&self) -> Option<DateTime<Utc>> {
        self.quota_cooldown(None).map(|cooldown| cooldown.until)
    }

    fn scoped_cooldown(&self, model: &str) -> Option<Cooldown> {
        model_cooldown_scope(self.lane, model).and_then(|scope| self.quota_cooldown(Some(&scope)))
    }

    fn quota_cooldown(&self, scope: Option<&str>) -> Option<Cooldown> {
        let settled = scope.map_or(self.cooldown, |scope| self.scoped_cooldowns.get(scope).copied());
        settled
            .into_iter()
            .chain(self.pending_quota.iter().filter(|pending| pending.scope.as_deref() == scope).filter_map(|pending| pending.provisional))
            .reduce(Cooldown::merge)
    }

    pub(crate) fn begin_google_quota_read(
        &mut self,
        model: &str,
        provisional: Option<Cooldown>,
        unverified_until: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> QuotaReader {
        self.pending_quota.retain(|pending| pending.blocks_verification(now));
        let reader = Arc::new(());
        self.pending_quota.push(PendingQuota {
            reader: Arc::downgrade(&reader),
            scope: model_cooldown_scope(self.lane, model),
            provisional: provisional.map(|cooldown| Cooldown { verified_reset: false, ..cooldown }),
            // A headerless fallback outlives a dropped reader for verification only.
            unverified_until,
        });
        QuotaReader(reader)
    }

    pub(crate) fn finish_google_quota_read(&mut self, reader: &QuotaReader, unverified_until: Option<DateTime<Utc>>) -> bool {
        let identity = Arc::downgrade(&reader.0);
        let Some(index) = self.pending_quota.iter().position(|pending| pending.reader.ptr_eq(&identity)) else { return false };
        if unverified_until.is_some() {
            let pending = &mut self.pending_quota[index];
            pending.reader = Weak::new();
            pending.provisional = None;
            pending.unverified_until = unverified_until;
        } else {
            self.pending_quota.swap_remove(index);
        }
        true
    }

    pub(crate) fn clear_quota_cooldowns(&mut self) {
        self.cooldown = None;
        self.scoped_cooldowns.clear();
        self.pending_quota.retain_mut(|pending| {
            pending.provisional = None;
            pending.unverified_until = None;
            pending.reader.strong_count() != 0
        });
    }

    pub(super) fn has_unverified_google_quota_read(&self, model: &str, now: DateTime<Utc>) -> bool {
        let scope = model_cooldown_scope(self.lane, model);
        // An unread or failed body still blocks verification until its own backoff expires,
        // even if a longer cooldown wins the legacy routing decision.
        self.pending_quota.iter().any(|pending| (pending.scope.is_none() || pending.scope == scope) && pending.blocks_verification(now))
    }

    fn active_quota_windows<'a>(&'a self, now: DateTime<Utc>, model: &'a str) -> impl Iterator<Item = &'a QuotaWindow> {
        self.windows
            .iter()
            .filter(move |w| window_applies_to_model(w, model) && enforces_quota(w) && quota_window_active(self.lane, w, now))
    }

    pub fn quota_limited_until(&self, now: DateTime<Utc>, model: &str) -> Option<DateTime<Utc>> {
        self.active_quota_windows(now, model)
            .map(|w| {
                w.resets_at.unwrap_or_else(|| {
                    if self.lane == Lane::Google {
                        w.observed_at.map_or(DateTime::<Utc>::MAX_UTC, |at| at + chrono::TimeDelta::hours(1))
                    } else {
                        DateTime::<Utc>::MAX_UTC
                    }
                })
            })
            .chain(self.quota_cooldown(None).into_iter().chain(self.scoped_cooldown(model)).map(|c| c.until).filter(|until| *until > now))
            .max()
    }

    pub fn hub_id(&self) -> String {
        format!("{}:{}", self.lane.family(), self.id)
    }

    pub fn hub_id_for(&self, host: &str, user: &str) -> String {
        format!("{}:{host}:{user}:{}", self.lane.family(), self.id)
    }

    pub fn inventory(&self, pool: &Pool) -> Value {
        let mut row = json!({
            "id": self.id, "name": self.name, "auth_index": self.auth_index, "provider": self.provider,
            "type": self.provider, "label": crate::error_classify::safe_label(self), "disabled": self.disabled, "note": self.note, "priority": self.priority,
            "weight": self.weight, "status": if self.disabled { "disabled" } else if self.has_account_error() { "error" } else { "active" },
            "cooldown_until": self.cooldown_until(), "observed_at": self.observed, "last_used_at": self.last_used,
            "last_error": self.reported_error(),
            "preferred": pool.preferred(self.lane) == Some(self.id.as_str()),
            "plan": self.plan.as_ref().map(|p| json!({"tier":p,"label":p,"multiplier":self.multiplier})),
        });
        if let Some(email) = &self.email {
            row["email"] = json!(email);
        }
        if self.lane == Lane::Google
            && !self.api_key
            && let Some(project) = &self.project
        {
            row["project_id"] = json!(project);
        }
        if self.lane == Lane::Codex && !self.api_key {
            let mut metadata = json!({});
            if let Some(id) = &self.chatgpt_id {
                metadata["chatgpt_account_id"] = json!(id);
            }
            if let Some(plan) = &self.plan {
                metadata["plan_type"] = json!(plan);
            }
            row["id_token"] = metadata;
        }
        if self.api_key {
            row["api_key"] = json!({"masked": "••••", "success": self.success, "failed": self.failure});
        }
        row
    }

    pub fn quota(&self, pool: &Pool) -> Value {
        let mut row = self.inventory(pool);
        row["windows"] = json!(self.windows.iter().map(|w| json!({
            "key": w.window_key, "label": w.label, "used_percent": w.used_percent,
            "resets_at": w.resets_at, "duration_seconds": w.window_duration_mins.map(|n| n.saturating_mul(60)),
            "scoped": w.scope_model.is_some() || w.kind == QuotaWindowKind::ModelScoped, "observed_at": w.observed_at, "stale": w.stale,
            "reset_cadence": match w.cadence { ResetCadence::Fixed => Some("fixed"), ResetCadence::RollingOnUse => Some("on_use"), _ => None },
            "status": match w.status { QuotaStatus::Allowed => Some("allowed"), QuotaStatus::Warning => Some("allowed_warning"),
                QuotaStatus::Rejected => Some("rejected"), _ => None }
        })).collect::<Vec<_>>());
        if let Some(first) = self.grants.first() {
            let now = Utc::now();
            let current = self.grants.iter().filter(|g| g.expires_at.is_none_or(|t| t > now)).collect::<Vec<_>>();
            row["reset_credits"] = json!({
                "available":current.iter().map(|g|u64::from(g.available_count)).sum::<u64>(),
                "applicable":current.iter().filter(|g| g.applicable).map(|g|u64::from(g.available_count)).sum::<u64>(),
                "checked_at":first.checked_at,
                "expiries":current.iter().filter_map(|g|g.expires_at).collect::<Vec<_>>(),
            });
            if self.lane == Lane::Claude {
                row["reset_credits"]["grants"] =
                    json!(current.iter().map(|g| json!({
                    "id":g.grant_id,"scope":g.scope,"available":g.available_count,
                    "applicable":if g.applicable {g.available_count} else {0},"expires_at":g.expires_at,"clears":g.cleared_windows,
                    "gives_back_weekly":g.weekly_credit_back,"unavailable_reason":g.unavailable_reason
                })).collect::<Vec<_>>());
            }
        }
        row
    }
}

pub(crate) fn text<'a>(raw: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| {
        let v = if key.starts_with('/') { raw.pointer(key) } else { raw.get(*key) }?;
        v.as_str().filter(|v| !v.trim().is_empty())
    })
}

pub(crate) fn jwt_claims(token: &str) -> Value {
    token
        .split('.')
        .nth(1)
        .filter(|payload| payload.len() < 32_768)
        .and_then(|payload| URL_SAFE_NO_PAD.decode(payload).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn date(v: &Value) -> Option<DateTime<Utc>> {
    v.as_str().and_then(|s| s.parse().ok()).or_else(|| {
        let n = v.as_i64()?;
        if n > 10_000_000_000 { DateTime::from_timestamp_millis(n) } else { DateTime::from_timestamp(n, 0) }
    })
}

pub fn filename(name: &str) -> bool {
    name.ends_with(".json")
        && name.len() <= 200
        && !name.starts_with('.')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-@+".contains(&b))
}

pub fn parse_credentials(name: &str, raw: &Value, local: bool) -> Result<Vec<Account>> {
    parse_credentials_for_lane(name, raw, local, None)
}

fn parse_credentials_for_lane(name: &str, raw: &Value, local: bool, known_lane: Option<Lane>) -> Result<Vec<Account>> {
    parse_credentials_with_options(name, raw, local, known_lane, false)
}

fn parse_credentials_with_options(
    name: &str,
    raw: &Value,
    local: bool,
    known_lane: Option<Lane>,
    force_api_key: bool,
) -> Result<Vec<Account>> {
    ensure!(filename(name) && raw.is_object(), "invalid credential name or JSON object");
    let api_key_wrapper = raw.get("api_key").filter(|v| v.is_object());
    let forced_api_key = force_api_key || api_key_wrapper.is_some();
    let group = api_key_wrapper.unwrap_or(raw);
    let lane = known_lane
        .or_else(|| text(raw, &["family", "provider", "type"]).and_then(Lane::parse))
        .or_else(|| text(group, &["family", "provider"]).and_then(Lane::parse))
        .or_else(|| text(raw, &["type"]).and_then(Lane::parse))
        .ok_or_else(|| anyhow!("unsupported credential provider"))?;
    let base = text(group, &["base_url", "base-url"]).or_else(|| text(raw, &["base_url", "base-url"])).map(origin).transpose()?;
    let models = group["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m.as_str().or_else(|| text(m, &["name", "id"])).map(str::to_owned))
        .collect::<Vec<_>>();
    let make = |entry: &Value, id: String, is_group: bool| -> Result<Account> {
        let key = entry.as_str().or_else(|| text(entry, &["api_key", "api-key", "key", "OPENAI_API_KEY"]));
        let access = key
            .or_else(|| {
                text(entry, &["access_token", "accessToken", "access", "token", "/tokens/access_token", "/claudeAiOauth/accessToken"])
            })
            .unwrap_or_default();
        let refresh = text(entry, &["refresh_token", "refreshToken", "refresh", "/tokens/refresh_token", "/claudeAiOauth/refreshToken"]);
        let google_client = text(entry, &["client_id", "clientId"]).map(str::to_owned).or_else(|| env::var("PROXY_GOOGLE_CLIENT_ID").ok());
        let google_secret =
            text(entry, &["client_secret", "clientSecret"]).map(str::to_owned).or_else(|| env::var("PROXY_GOOGLE_CLIENT_SECRET").ok());
        ensure!(
            !access.is_empty() || (lane == Lane::Google && refresh.is_some() && google_client.is_some() && google_secret.is_some()),
            "credential has no usable access or refresh token"
        );
        ensure!(access.len() <= 32_768 && access.bytes().all(|b| b.is_ascii_graphic()), "invalid credential token");
        let (raw_api, raw_oauth) = record_api_key_evidence(raw, lane);
        let (entry_api, entry_oauth) = record_api_key_evidence(entry, lane);
        let explicit_oauth = raw_oauth || entry_oauth;
        let marker_api_key = raw_api || entry_api;
        let bool_api_key =
            [raw, entry].into_iter().any(|value| value["api_key"].as_bool() == Some(true) || value["api-key"].as_bool() == Some(true));
        ensure!(
            api_key_wrapper.is_some() || !(bool_api_key || force_api_key || marker_api_key) || (refresh.is_none() && !explicit_oauth),
            "contradictory api_key and OAuth credential fields"
        );
        let api_key = lane != Lane::Go && (forced_api_key || bool_api_key || marker_api_key || key.is_some() || is_group);
        let codex_oauth_explicit = lane == Lane::Codex
            && !api_key
            && [raw, entry]
                .into_iter()
                .flat_map(|value| {
                    ["type", "provider", "auth_type", "auth_mode"]
                        .into_iter()
                        .filter_map(|field| text(value, &[field]).map(str::to_ascii_lowercase))
                })
                .any(|kind| matches!(kind.as_str(), "oauth" | "oauth2" | "chatgpt" | "chatgptauthtokens"));
        let access_claims = jwt_claims(access);
        let id_token = text(entry, &["id_token", "/tokens/id_token"]);
        let claims = id_token.map(jwt_claims).unwrap_or_else(|| access_claims.clone());
        let chat = &claims["https://api.openai.com/auth"];
        let access_chat = &access_claims["https://api.openai.com/auth"];
        let chatgpt_id =
            text(entry, &["account_id", "accountId", "chatgpt_account_id", "/tokens/account_id", "/id_token/chatgpt_account_id"])
                .or_else(|| text(access_chat, &["chatgpt_account_id"]))
                .or_else(|| text(chat, &["chatgpt_account_id"]))
                .map(str::to_owned);
        let chatgpt_user = text(access_chat, &["chatgpt_user_id"]).or_else(|| text(chat, &["chatgpt_user_id"])).map(str::to_owned);
        let organization = text(entry, &["organization_uuid", "organizationUuid", "/claudeAiOauth/organizationUuid"]).map(str::to_owned);
        let plan =
            text(entry, &["plan", "plan_type", "/id_token/plan_type", "/id_token/chatgpt_plan_type", "/claudeAiOauth/subscriptionType"])
                .or_else(|| text(chat, &["chatgpt_plan_type"]))
                .map(str::to_owned);
        let entry_obj = entry.as_object();
        let disabled = match entry_obj
            .and_then(|o| o.get("disabled").and_then(Value::as_bool))
            .or_else(|| entry_obj.and_then(|o| o.get("enabled").and_then(Value::as_bool)).map(|e| !e))
        {
            Some(d) => d,
            None => [raw, group].into_iter().any(|v| v["disabled"].as_bool() == Some(true) || v["enabled"].as_bool() == Some(false)),
        };
        let priority = entry_obj
            .and_then(|o| o.get("priority").and_then(Value::as_i64))
            .or_else(|| group["priority"].as_i64())
            .or_else(|| raw["priority"].as_i64())
            .and_then(|n| i32::try_from(n).ok())
            .unwrap_or_default();
        let weight = entry_obj
            .and_then(|o| o.get("weight").and_then(Value::as_u64))
            .or_else(|| group["weight"].as_u64())
            .or_else(|| raw["weight"].as_u64())
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(1);
        let note = match entry_obj.and_then(|o| o.get("note")) {
            Some(Value::Null) => None,
            Some(Value::String(s)) if !s.trim().is_empty() => Some(s.clone()),
            _ => text(group, &["note"]).or_else(|| text(raw, &["note"])).map(str::to_owned),
        };
        let mut account = Account {
            auth_index: hex::encode(&Sha256::digest(format!("fleetix-proxy:{id}"))[..12]),
            id,
            name: name.into(),
            lane,
            provider: if api_key && lane == Lane::Codex { "openai" } else { lane.name() },
            label: text(entry, &["label"]).or_else(|| text(raw, &["label", "name"])).unwrap_or(name).into(),
            email: text(entry, &["email"]).or_else(|| text(&claims, &["email"])).map(str::to_owned),
            note,
            plan,
            disabled,
            priority,
            weight,
            multiplier: raw["tier_multiplier"]
                .as_f64()
                .or_else(|| raw.pointer("/plan/multiplier").and_then(Value::as_f64))
                .filter(|n| *n > 0.0),
            access: access.into(),
            refresh: refresh.map(str::to_owned),
            redacted_labels: HashMap::new(),
            expires: ["expires_at", "expiresAt", "expiry", "expired", "expires", "/claudeAiOauth/expiresAt"]
                .into_iter()
                .find_map(|k| date(if k.starts_with('/') { entry.pointer(k).unwrap_or(&Value::Null) } else { &entry[k] }))
                .or_else(|| date(&jwt_claims(access)["exp"])),
            chatgpt_id,
            chatgpt_user,
            organization,
            google_client,
            google_secret,
            provenance_enrollment: None,
            provenance_project: None,
            project: text(entry, &["project_id", "projectId", "project"]).map(str::to_owned),
            api_key,
            codex_oauth_explicit,
            base: (api_key || lane == Lane::Go).then(|| base.clone()).flatten(),
            local,
            models: models.clone(),
            cooldown: None,
            last_error: None,
            quota_probe_policy_error: None,
            error_generation: 0,
            source_fingerprint: None,
            scoped_cooldowns: BTreeMap::new(),
            pending_quota: Vec::new(),
            windows: vec![],
            google_fallback_keys: BTreeSet::new(),
            grants: vec![],
            next_grant: None,
            observed: None,
            last_used: None,
            probe_failed: false,
            probe_generation: 0,
            provenance_token_fp: None,
            success: 0,
            failure: 0,
        };
        // Hash parsed credential material, not the editable credential document.
        let material = json!([
            account.access,
            account.refresh,
            account.codex_oauth_explicit,
            account.expires,
            id_token,
            account.google_client,
            account.google_secret,
            account.chatgpt_id,
            account.chatgpt_user,
            account.organization,
            account.project,
        ]);
        account.source_fingerprint = Some(hex::encode(Sha256::digest(material.to_string())));
        Ok(account)
    };
    if let Some(keys) = group["keys"].as_array() {
        ensure!(!keys.is_empty() && keys.len() <= 256, "invalid key group size");
        keys.iter().enumerate().map(|(i, entry)| make(entry, format!("{name}#{}", i + 1), true)).collect()
    } else {
        Ok(vec![make(group, name.into(), false)?])
    }
}

type Session = (Lane, String);

#[derive(Serialize)]
pub(crate) struct ModelEligibility<'a> {
    snapshot_at: DateTime<Utc>,
    lane: &'static str,
    wire_model: &'a str,
    supported_accounts: usize,
    eligible_accounts: usize,
    verified_next_reset_at: Option<DateTime<Utc>>,
    accounts: Vec<AccountEligibility<'a>>,
}

#[derive(Serialize)]
struct AccountEligibility<'a> {
    auth_index: &'a str,
    exclusion_reasons: Vec<&'static str>,
}

pub(crate) struct AccountFilter<'a> {
    pub lane: Lane,
    pub model: &'a str,
    pub required_project: Option<&'a str>,
    pub subscriptions_only: bool,
    pub require_account_project: bool,
}

impl<'a> AccountFilter<'a> {
    pub(crate) fn shared(lane: Lane, model: &'a str, required_project: Option<&'a str>) -> Self {
        Self { lane, model, required_project, subscriptions_only: false, require_account_project: false }
    }

    pub(crate) fn allows(&self, account: &Account) -> bool {
        account.lane == self.lane
            && (!self.require_account_project || account.project.as_deref().is_some_and(|project| !project.trim().is_empty()))
            && (!self.subscriptions_only
                || match self.lane {
                    Lane::Codex => account.is_codex_oauth(),
                    Lane::Google => account.is_google_oauth(),
                    _ => false,
                })
    }

    pub(crate) fn allows_in(&self, pool: &Pool, account: &Account) -> bool {
        self.allows(account) && (!self.subscriptions_only || !pool.is_api_key_conflicted(account))
    }

    pub(crate) fn supports(&self, pool: &Pool, account: &Account) -> bool {
        self.allows_in(pool, account) && pool.supports(account, self.lane, self.model, self.required_project)
    }
}

#[derive(Default)]
pub struct Pool {
    pub accounts: BTreeMap<String, Account>,
    pub policy: RoutingPolicy,
    pub overrides: BTreeMap<Lane, String>,
    pub cursors: HashMap<(Lane, bool, Vec<String>), u64>,
    pub revision: i64,
    pub source_host: String,
    pub source_user: String,
    affinity: HashMap<Session, (String, Instant)>,
    parents: HashMap<Session, String>,
    api_key_conflicts: BTreeSet<(Lane, [u8; 32])>,
}

impl Pool {
    pub(crate) fn record_api_key_conflict(&mut self, account: &Account) {
        let mut changed = false;
        for fp in account.credential_fingerprints() {
            changed |= self.api_key_conflicts.insert((account.lane, fp));
        }
        if changed {
            self.revision += 1;
        }
    }

    pub(crate) fn collect_api_key_conflict_fps(&self) -> Vec<(Lane, [u8; 32])> {
        let mut out: BTreeSet<(Lane, [u8; 32])> = self.api_key_conflicts.clone();
        for account in self.accounts.values().filter(|a| a.api_key) {
            for fp in account.credential_fingerprints() {
                out.insert((account.lane, fp));
            }
        }
        out.into_iter().collect()
    }

    pub(crate) fn merge_api_key_conflict_fps(&mut self, fps: &[(Lane, [u8; 32])]) {
        let mut changed = false;
        for entry in fps {
            changed |= self.api_key_conflicts.insert(*entry);
        }
        if changed {
            self.revision += 1;
        }
        self.propagate_api_key_conflicts();
    }

    fn propagate_api_key_conflicts(&mut self) {
        // Propagate lane-scoped fingerprints to a fixed point so transitive aliases
        // (e.g., A shares access with API key -> shared refresh -> B's access) are
        // all persisted regardless of discovery or insertion order.
        loop {
            let mut added = false;
            let fps_to_add: Vec<(Lane, [u8; 32])> = self
                .accounts
                .values()
                .filter(|acc| {
                    acc.credential_fingerprints().iter().any(|fp| self.api_key_conflicts.contains(&(acc.lane, *fp)))
                        || (acc.api_key
                            && self
                                .accounts
                                .values()
                                .any(|other| other.id != acc.id && !other.api_key && other.shares_credential_fingerprint(acc)))
                })
                .flat_map(|acc| acc.credential_fingerprints().into_iter().map(move |fp| (acc.lane, fp)))
                .collect();
            for entry in fps_to_add {
                added |= self.api_key_conflicts.insert(entry);
            }
            if !added {
                break;
            }
            self.revision += 1;
        }
    }

    pub(crate) fn is_api_key_conflicted(&self, account: &Account) -> bool {
        let fps = account.credential_fingerprints();
        if fps.iter().any(|fp| self.api_key_conflicts.contains(&(account.lane, *fp))) {
            return true;
        }
        self.accounts.values().any(|other| {
            other.id != account.id
                && other.lane == account.lane
                && other.api_key
                && (other.shares_discovery_token(account) || other.credential_fingerprints().iter().any(|fp| fps.contains(fp)))
        })
    }
    pub fn set_source(&mut self, host: String, user: String) {
        self.source_host = host;
        self.source_user = user;
    }

    pub fn hub_id(&self, account: &Account) -> String {
        if self.source_host.is_empty() && self.source_user.is_empty() {
            account.hub_id()
        } else {
            account.hub_id_for(&self.source_host, &self.source_user)
        }
    }
    pub fn load(config: &Config) -> Result<Self> {
        let mut pool = Self::default();
        for path in paths(&config.auth_dir) {
            let Some(name) = path.file_name().and_then(|n| n.to_str()).filter(|n| filename(n)) else { continue };
            if !path.is_file() {
                continue;
            }
            if let Some(raw) = read_json(&path)
                && let Ok(accounts) = parse_credentials(name, &raw, false)
            {
                pool.insert(accounts);
            } else {
                tracing::warn!("ignoring invalid credential file");
            }
        }
        if config.discover_local {
            discover(config, &mut pool);
        }
        Ok(pool)
    }

    #[cfg(test)]
    pub fn reload_local(config: &Config, account: &Account) -> Option<Account> {
        Self::reload_local_with_conflicts(config, account).0
    }

    pub(crate) fn reload_local_with_conflicts(config: &Config, account: &Account) -> (Option<Account>, Vec<(Lane, [u8; 32])>) {
        if config.exclude_local_lanes.contains(&account.lane) {
            return (None, Vec::new());
        }
        let mut found = Self::default();
        match account.lane {
            Lane::Claude => discover_claude(config, &mut found),
            Lane::Codex => discover_codex(config, &mut found),
            _ => return (None, Vec::new()),
        }
        discover_opencode(config, &mut found);
        let conflicts = found.collect_api_key_conflict_fps();
        let reloaded = found.accounts.get(&account.id).filter(|a| !found.is_api_key_conflicted(a)).cloned();
        (reloaded, conflicts)
    }

    pub fn reload_file(config: &Config, account: &Account) -> Option<Account> {
        if !filename(&account.name) {
            return None;
        }
        let raw = read_json(&config.auth_dir.join(&account.name))?;
        parse_credentials(&account.name, &raw, false).ok()?.into_iter().find(|a| a.id == account.id)
    }

    pub fn insert(&mut self, accounts: Vec<Account>) {
        for mut a in accounts {
            a.pending_quota.clear();
            // Record BOTH rows' fingerprints symmetrically whenever an API-key row and an
            // OAuth row share any lane-scoped credential fingerprint (including saved
            // provenance_token_fp after access refresh), regardless of order or ID match.
            let conflicted_others: Vec<Account> = self
                .accounts
                .values()
                .filter(|other| other.api_key != a.api_key && other.shares_credential_fingerprint(&a))
                .cloned()
                .collect();
            if !conflicted_others.is_empty() || a.credential_fingerprints().iter().any(|fp| self.api_key_conflicts.contains(&(a.lane, *fp)))
            {
                for other in &conflicted_others {
                    self.record_api_key_conflict(other);
                }
                self.record_api_key_conflict(&a);
            }
            if let Some(old) = self.accounts.get_mut(&a.id) {
                a.redacted_labels.extend(std::mem::take(&mut old.redacted_labels));
                // Scrub before preserving any live credentials on an unchanged reload.
                a.scrub_label(&old.current_secrets());
                if a.local && old.local && old.lane == a.lane {
                    a.disabled = old.disabled;
                    a.priority = old.priority;
                    a.weight = old.weight;
                    a.note.clone_from(&old.note);
                }
                let unchanged = a.lane == old.lane
                    && a.api_key == old.api_key
                    && a.base == old.base
                    && (a.lane != Lane::Google || a.project.is_none() || a.project == old.project)
                    && if a.source_fingerprint.is_some() { a.source_fingerprint == old.source_fingerprint } else { old.same_secret(&a) };
                if unchanged {
                    a.access.clone_from(&old.access);
                    a.refresh.clone_from(&old.refresh);
                    a.expires = old.expires;
                    a.google_client.clone_from(&old.google_client);
                    a.google_secret.clone_from(&old.google_secret);
                    a.project.clone_from(&old.project);
                    a.provenance_project.clone_from(&old.provenance_project);
                    if a.email.is_none() {
                        a.email.clone_from(&old.email);
                    }
                    if a.local {
                        a.label = crate::error_classify::safe_label(old);
                    }
                    a.cooldown = old.cooldown;
                    a.last_error.clone_from(&old.last_error);
                    a.quota_probe_policy_error.clone_from(&old.quota_probe_policy_error);
                    a.scoped_cooldowns.clone_from(&old.scoped_cooldowns);
                    a.pending_quota.clone_from(&old.pending_quota);
                    a.windows.clone_from(&old.windows);
                    a.google_fallback_keys.clone_from(&old.google_fallback_keys);
                    a.grants.clone_from(&old.grants);
                    a.next_grant.clone_from(&old.next_grant);
                    a.observed = old.observed;
                    a.last_used = old.last_used;
                    a.probe_failed = old.probe_failed;
                    a.probe_generation = old.probe_generation;
                    a.provenance_token_fp = old.provenance_token_fp;
                    a.success = old.success;
                    a.failure = old.failure;
                } else if a.lane == Lane::Google && (a.project != old.project || a.base != old.base) {
                    a.clear_google_route_evidence();
                }
                a.error_generation = old.error_generation.saturating_add(u64::from(!unchanged));
            } else {
                a.scrub_label(&[]);
            }
            self.accounts.insert(a.id.clone(), a);
        }
        self.propagate_api_key_conflicts();
    }

    pub fn find(&self, id: &str) -> Option<&Account> {
        self.accounts.get(id).or_else(|| self.accounts.values().find(|a| a.auth_index == id || self.hub_id(a) == id || a.hub_id() == id))
    }

    fn support_exclusion(&self, account: &Account, model: &str, required_project: Option<&str>) -> Option<&'static str> {
        if required_project.is_some_and(|project| account.project.as_deref() != Some(project)) {
            return Some("project_mismatch");
        }
        account
            .support_exclusion(model)
            .or_else(|| (account.api_key && self.policy.api_key_order == ApiKeyOrder::SubscriptionsOnly).then_some("subscriptions_only"))
    }

    pub(crate) fn supports(&self, a: &Account, lane: Lane, model: &str, required_project: Option<&str>) -> bool {
        a.lane == lane && self.support_exclusion(a, model, required_project).is_none()
    }

    fn supported<'a>(&'a self, filter: &'a AccountFilter<'a>) -> impl Iterator<Item = &'a Account> {
        self.accounts.values().filter(move |a| filter.supports(self, a))
    }

    pub(crate) fn evaluate_model_eligibility<'a>(
        &'a self,
        lane: Lane,
        model: &'a str,
        required_project: Option<&'a str>,
        now: DateTime<Utc>,
    ) -> Option<ModelEligibility<'a>> {
        self.evaluate_model_eligibility_for(&AccountFilter::shared(lane, model, required_project), now)
    }

    pub(crate) fn evaluate_model_eligibility_for<'a>(
        &'a self,
        filter: &AccountFilter<'a>,
        now: DateTime<Utc>,
    ) -> Option<ModelEligibility<'a>> {
        let lane = filter.lane;
        let model = filter.model;
        let required_project = filter.required_project;
        if !bounded_identifier(model) {
            return None;
        }
        let horizon = chrono::TimeDelta::seconds(MAX_COOLDOWN_SECS);
        let mut evaluation = ModelEligibility {
            snapshot_at: now,
            lane: lane.name(),
            wire_model: model,
            supported_accounts: 0,
            eligible_accounts: 0,
            verified_next_reset_at: None,
            accounts: Vec::new(),
        };
        for account in self.accounts.values().filter(|a| a.lane == lane) {
            let mut reasons = Vec::new();
            if !filter.allows_in(self, account) {
                reasons.push("subscriptions_only");
            } else if let Some(reason) = self.support_exclusion(account, model, required_project) {
                reasons.push(reason);
            } else {
                evaluation.supported_accounts += 1;
                let mut account_reset = None;
                let mut complete = true;
                let mut block = |reason, reset_at: Option<DateTime<Utc>>| {
                    reasons.push(reason);
                    if let Some(at) = reset_at.filter(|at| *at > now && *at - now <= horizon) {
                        account_reset = account_reset.max(Some(at));
                    } else {
                        complete = false;
                    }
                };
                if let Some(error) = account.active_quarantine(now) {
                    let reason = if error.kind == AccountErrorKind::AuthExpired { "auth_cooldown" } else { "quarantined" };
                    block(reason, None);
                }
                for (reason, cooldown) in
                    [("global_cooldown", account.quota_cooldown(None)), ("scoped_cooldown", account.scoped_cooldown(model))]
                {
                    if let Some(c) = cooldown.filter(|c| c.until > now) {
                        block(reason, c.verified_reset.then_some(c.until));
                    }
                }
                let mut windows = account.active_quota_windows(now, model).peekable();
                if windows.peek().is_some() {
                    // A cached reset must have been credible when observed, not just when read.
                    let reset = windows.try_fold(now, |latest, w| {
                        let (observed, at) = (w.observed_at?, w.resets_at?);
                        (observed <= now && observed < at && at - observed <= horizon).then_some(latest.max(at))
                    });
                    block("quota_window", reset);
                }
                if reasons.is_empty() {
                    evaluation.eligible_accounts += 1;
                } else if complete
                    && !account.has_unverified_google_quota_read(model, now)
                    && let Some(reset) = account_reset
                {
                    evaluation.verified_next_reset_at = Some(evaluation.verified_next_reset_at.map_or(reset, |old| old.min(reset)));
                }
            }
            evaluation.accounts.push(AccountEligibility { auth_index: &account.auth_index, exclusion_reasons: reasons });
        }
        if evaluation.eligible_accounts > 0 {
            evaluation.verified_next_reset_at = None;
        }
        Some(evaluation)
    }

    #[cfg(test)]
    pub(crate) fn verified_google_reset_at(
        &self,
        model: &str,
        required_project: Option<&str>,
        now: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        self.verified_google_reset_at_for(&AccountFilter::shared(Lane::Google, model, required_project), now)
    }

    pub(crate) fn verified_google_reset_at_for(&self, filter: &AccountFilter<'_>, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.evaluate_model_eligibility_for(filter, now)?.verified_next_reset_at
    }

    pub fn next_reset(&self, filter: &AccountFilter<'_>, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.supported(filter).filter(|a| !a.quarantined(now)).filter_map(|a| a.quota_limited_until(now, filter.model)).min()
    }

    pub fn all_quota_limited(&self, filter: &AccountFilter<'_>, now: DateTime<Utc>) -> bool {
        let mut accounts = self.supported(filter).peekable();
        accounts.peek().is_some() && accounts.all(|a| !a.quarantined(now) && a.quota_limited_until(now, filter.model).is_some())
    }

    pub(crate) fn has_eligible_account(&self, filter: &AccountFilter<'_>, now: DateTime<Utc>) -> bool {
        self.supported(filter).any(|a| a.eligible(now, filter.model))
    }

    pub(crate) fn uniform_quarantine_kind(&self, filter: &AccountFilter<'_>, now: DateTime<Utc>) -> Option<AccountErrorKind> {
        let mut kinds = self.supported(filter).map(|a| a.active_quarantine(now).map(|err| err.kind));
        let kind = kinds.next()??;
        kinds.all(|other| other == Some(kind)).then_some(kind)
    }

    pub fn preferred(&self, lane: Lane) -> Option<&str> {
        self.overrides
            .get(&lane)
            .map(String::as_str)
            .or(self.policy.preferred_account_id.as_deref())
            .and_then(|id| self.find(id))
            .filter(|a| a.lane == lane && !a.disabled)
            .map(|a| a.id.as_str())
    }

    pub fn pick(
        &mut self,
        lane: Lane,
        affinity: (Option<&str>, Option<&str>),
        model: &str,
        required_project: Option<&str>,
        tried: &[String],
        now: DateTime<Utc>,
    ) -> Option<Account> {
        self.pick_for(&AccountFilter::shared(lane, model, required_project), affinity, tried, now)
    }

    pub(crate) fn pick_for(
        &mut self,
        filter: &AccountFilter<'_>,
        (session, parent): (Option<&str>, Option<&str>),
        tried: &[String],
        now: DateTime<Utc>,
    ) -> Option<Account> {
        let AccountFilter { lane, model, .. } = *filter;
        let eligible = |a: &&Account| filter.supports(self, a) && a.eligible(now, model) && !tried.contains(&a.id);
        if let Some(a) = self.preferred(lane).and_then(|id| self.accounts.get(id)).filter(eligible) {
            return Some(a.clone());
        }
        let mut options: Vec<_> = self.accounts.values().filter(eligible).collect();
        let keys_first = self.policy.api_key_order == ApiKeyOrder::KeysFirst;
        if options.iter().any(|a| a.api_key == keys_first) {
            options.retain(|a| a.api_key == keys_first);
        }
        let bound = |session| {
            self.bound(lane, session)
                .filter(|id| !filter.subscriptions_only || self.accounts.get(*id).is_some_and(|a| filter.allows_in(self, a)))
        };
        let soonest = |a: &Account| {
            let reset = |applies: fn(&QuotaWindow) -> bool| {
                a.windows
                    .iter()
                    .filter(|w| applies(w) && window_applies_to_model(w, model))
                    .filter_map(|w| w.resets_at)
                    .filter(|t| *t > now)
                    .min()
                    .unwrap_or(DateTime::<Utc>::MAX_UTC)
            };
            let weekly = reset(|w| {
                w.kind == QuotaWindowKind::Weekly
                    || (w.kind == QuotaWindowKind::ModelScoped && w.window_duration_mins.is_none_or(|m| m >= 1440))
            });
            let session = reset(|w| w.kind == QuotaWindowKind::Session);
            (weekly, session, std::cmp::Reverse(a.priority))
        };
        // Antigravity subscriptions drain the soonest weekly reset first: a session bound to a
        // later-resetting account moves back once that account's 5-hour window frees up.
        let drain_soonest = lane == Lane::Google && self.policy.strategy == RouteStrategy::SoonestReset;
        let sticky = session.and_then(bound).or_else(|| parent.and_then(bound));
        if let Some(a) = sticky.and_then(|id| options.iter().find(|a| a.id == id))
            && (!drain_soonest || options.iter().all(|b| soonest(b).0 >= soonest(a).0))
        {
            return Some((*a).clone());
        }
        let first = *options.first()?;
        match self.policy.strategy {
            RouteStrategy::SoonestReset => options.sort_by_key(|a| soonest(a)),
            RouteStrategy::FillFirst => options.sort_by(|a, b| {
                let used = |a: &Account| {
                    a.windows
                        .iter()
                        .filter(|w| matches!(w.kind, QuotaWindowKind::Weekly | QuotaWindowKind::Session | QuotaWindowKind::ModelScoped))
                        .filter(|w| window_applies_to_model(w, model))
                        .filter(|w| quota_window_active(a.lane, w, now))
                        .filter_map(|w| w.used_percent)
                        .fold(0.0_f64, f64::max)
                };
                b.priority.cmp(&a.priority).then_with(|| used(b).total_cmp(&used(a))).then_with(|| b.success.cmp(&a.success))
            }),
            RouteStrategy::RoundRobin | RouteStrategy::WeightedRoundRobin => {
                let weighted = self.policy.strategy == RouteStrategy::WeightedRoundRobin;
                let weight = |a: &&Account| if weighted { u64::from(a.weight) } else { 1 };
                let total: u64 = options.iter().map(weight).sum();
                let key = (lane, first.api_key, options.iter().map(|a| a.id.clone()).collect());
                let mut slot = self.cursors.get(&key).copied().unwrap_or_default() % total;
                if tried.is_empty() {
                    // ponytail: arbitrary eviction at the fixed ceiling; use LRU if account sets churn.
                    if self.cursors.len() >= 256
                        && !self.cursors.contains_key(&key)
                        && let Some(old) = self.cursors.keys().next().cloned()
                    {
                        self.cursors.remove(&old);
                    }
                    let cursor = self.cursors.entry(key).or_default();
                    *cursor = cursor.wrapping_add(1);
                }
                return options
                    .into_iter()
                    .find(|a| {
                        let w = weight(a);
                        if slot < w {
                            true
                        } else {
                            slot -= w;
                            false
                        }
                    })
                    .cloned();
            }
        }
        options.first().map(|a| (*a).clone())
    }

    pub fn preview(&self, lane: Lane) -> Option<String> {
        let mut copy = Self {
            accounts: self.accounts.clone(),
            policy: self.policy.clone(),
            overrides: self.overrides.clone(),
            cursors: self.cursors.clone(),
            ..Self::default()
        };
        copy.pick(lane, (None, None), "", None, &[], Utc::now()).map(|a| a.id)
    }

    pub fn bound(&self, lane: Lane, id: &str) -> Option<&str> {
        let ttl = Duration::from_secs(self.policy.session_affinity_secs);
        self.affinity.get(&(lane, id.into())).filter(|(_, seen)| seen.elapsed() < ttl).map(|(id, _)| id.as_str())
    }

    pub(crate) fn clear_affinity(&mut self) {
        self.affinity.clear();
    }

    pub fn bind(&mut self, lane: Lane, session: &str, account: &str) {
        if self.policy.session_affinity_secs == 0 {
            return;
        }
        // ponytail: evict the oldest at the fixed ceiling; use a timed LRU if this is hot.
        let key = (lane, session.to_owned());
        if self.affinity.len() >= 4096
            && !self.affinity.contains_key(&key)
            && let Some(old) = self.affinity.iter().min_by_key(|(_, (_, at))| *at).map(|(k, _)| k.clone())
        {
            self.affinity.remove(&old);
            self.parents.remove(&old);
        }
        self.affinity.insert(key, (account.into(), Instant::now()));
    }

    pub fn link(&mut self, lane: Lane, child: &str, parent: &str) {
        // Links exist only for bound sessions, so the same eviction cap applies.
        if child != parent && self.bound(lane, child).is_some() {
            self.parents.insert((lane, child.into()), parent.into());
        }
    }

    pub fn move_session(&mut self, session: &str, id: &str, include_subagents: bool) -> Result<usize, &'static str> {
        if self.policy.session_affinity_secs == 0 {
            return Err("session affinity is disabled");
        }
        let account = self.find(id).filter(|a| !a.disabled).ok_or("account not found")?;
        let (lane, id) = (account.lane, account.id.clone());
        if session.starts_with('#') || self.bound(lane, session).is_none() {
            return Err("session not found");
        }
        let mut moved = BTreeSet::new();
        let mut pending = vec![session.to_owned()];
        while let Some(node) = pending.pop() {
            if !moved.insert(node.clone()) {
                continue;
            }
            if include_subagents {
                pending.extend(
                    self.parents
                        .iter()
                        .filter(|((provider, child), parent)| *provider == lane && *parent == &node && self.bound(lane, child).is_some())
                        .map(|((_, child), _)| child.clone()),
                );
            }
        }
        for session in &moved {
            if let Some((bound, _)) = self.affinity.get_mut(&(lane, session.clone())) {
                bound.clone_from(&id);
            }
        }
        Ok(moved.len())
    }
}

fn paths(dir: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<_> = fs::read_dir(dir).into_iter().flatten().filter_map(|entry| entry.ok().map(|e| e.path())).collect();
    paths.sort();
    paths
}

fn read_json(path: &Path) -> Option<Value> {
    if !fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() <= 2 * 1024 * 1024) {
        return None;
    }
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn local_credential_name(lane: Lane, label: &str) -> String {
    format!("local-{}-{}.json", lane.name(), hex::encode(&Sha256::digest(label)[..8]))
}

fn add(pool: &mut Pool, lane: Lane, label: &str, raw: Value) {
    add_with_api_key(pool, lane, label, raw, false);
}

fn add_with_api_key(pool: &mut Pool, lane: Lane, label: &str, raw: Value, force_api_key: bool) {
    let name = local_credential_name(lane, label);
    if let Ok(found) = parse_credentials_with_options(&name, &raw, true, Some(lane), force_api_key) {
        let cutoff = Utc::now() + chrono::TimeDelta::seconds(30);
        let mut api_key_conflicts = Vec::new();
        let found = found
            .into_iter()
            .filter_map(|mut a| {
                if !a.api_key && matches!(a.lane, Lane::Claude | Lane::Codex) && a.expires.is_some_and(|e| e <= cutoff) {
                    return None;
                }
                let conflicted_existing: Vec<Account> = pool
                    .accounts
                    .values()
                    .filter(|old| old.api_key != a.api_key && old.shares_credential_fingerprint(&a))
                    .cloned()
                    .collect();
                if !conflicted_existing.is_empty() {
                    for old in conflicted_existing {
                        api_key_conflicts.push(old);
                    }
                    api_key_conflicts.push(a.clone());
                }
                let mut duplicates = pool.accounts.values_mut().filter(|old| old.id != a.id && old.shares_discovery_token(&a)).peekable();
                if duplicates.peek().is_none() {
                    return Some(a);
                }
                if a.api_key {
                    // Retain API-key classification on the shared token identity regardless of discovery order.
                    api_key_conflicts.push(a);
                    return None;
                }
                if a.lane != Lane::Google {
                    return None;
                }
                if let Some(survivor) = duplicates.find(|old| old.compatible_refresh(&a)) {
                    if survivor.api_key {
                        api_key_conflicts.push(a);
                    } else if survivor.refresh.is_none() && a.refresh.is_some() {
                        merge_google_duplicate(survivor, a, false, false);
                        pool.revision += 1;
                    }
                    return None;
                }
                a.discard_discovered_access();
                Some(a)
            })
            .collect();
        for conflicted in &api_key_conflicts {
            pool.record_api_key_conflict(conflicted);
        }
        pool.insert(found);
    }
}

pub(crate) fn provenance_google_manifests(config: &Config, user: &str) -> Vec<(String, PathBuf)> {
    if user.is_empty() {
        return Vec::new();
    }
    let prefix = format!("{user}-");
    paths(&config.provenance_oauth_dir)
        .into_iter()
        .filter_map(|path| {
            let name = path.file_name()?.to_str()?;
            if !name.starts_with(&prefix) || !name.ends_with(".json") {
                return None;
            }
            let raw = read_json(&path)?;
            if text(&raw, &["/target/provider"]) != Some("google") || text(&raw, &["/target/profile"]) != Some("antigravity") {
                return None;
            }
            let stem = path.file_stem()?.to_str()?;
            let enrollment = stem.strip_prefix(&prefix).unwrap_or(stem);
            let label = text(&raw, &["/target/enrollment"]).unwrap_or(enrollment).to_owned();
            Some((label, path))
        })
        .collect()
}

fn merge_google_duplicate(survivor: &mut Account, replacement: Account, explicit_client: bool, explicit_secret: bool) {
    if survivor.api_key || replacement.api_key || survivor.access.is_empty() || survivor.access != replacement.access {
        return;
    }
    let adopting_refresh = survivor.refresh.is_none() && replacement.refresh.is_some();
    // Adoption also makes the refresh tokens match. Otherwise an access-token
    // match alone does not establish that the OAuth client is the same.
    if adopting_refresh || survivor.refresh == replacement.refresh {
        if adopting_refresh {
            survivor.refresh = replacement.refresh;
        }
        let fresher = replacement.expires.zip(survivor.expires).is_some_and(|(new, old)| new > old);
        if adopting_refresh || survivor.google_client.is_none() || (fresher && explicit_client) {
            survivor.google_client = replacement.google_client;
        }
        if adopting_refresh || survivor.google_secret.is_none() || (fresher && explicit_secret) {
            survivor.google_secret = replacement.google_secret;
        }
    }
    survivor.expires = survivor.expires.max(replacement.expires);
    survivor.scrub_label(&[]);
}

fn classify_google_provenance_api_key(raw: &Value) -> Result<bool> {
    ensure!(raw.is_object(), "invalid provenance Google credential object");
    let (is_api_key, explicit_oauth) = record_api_key_evidence(raw, Lane::Google);
    ensure!(!is_api_key || !explicit_oauth, "contradictory api_key and OAuth credential fields");
    Ok(is_api_key)
}

pub(crate) fn add_provenance_google(pool: &mut Pool, label: &str, mut raw: Value) {
    // Validate the original record before rewriting wrapper metadata so explicit API-key
    // markers or contradictory fields cannot be masked during enrollment or rediscovery.
    let Ok(candidate_api_key) = classify_google_provenance_api_key(&raw) else { return };
    let Some(fields) = raw.as_object_mut() else { return };
    fields.entry("label").or_insert_with(|| json!(label));
    if !candidate_api_key {
        fields.insert("type".into(), json!(Lane::Google.name()));
    }
    fields.remove("provider");
    fields.remove("family");
    let refresh = text(&raw, &["refresh_token", "refreshToken", "refresh"]).map(str::to_owned);
    let client = text(&raw, &["client_id", "clientId"]).map(str::to_owned);
    let secret = text(&raw, &["client_secret", "clientSecret"]).map(str::to_owned);
    let project = text(&raw, &["project_id", "projectId", "project"]).map(str::to_owned);
    let email = text(&raw, &["email"]).map(str::to_owned);
    let enrolled = pool.accounts.values().find(|a| a.provenance_enrollment.as_deref() == Some(label));
    let existing = enrolled.or_else(|| {
        if candidate_api_key {
            return None;
        }
        let refresh = refresh.as_ref()?;
        pool.accounts.values().find(|a| a.lane == Lane::Google && !a.api_key && a.refresh.as_ref() == Some(refresh))
    });
    if let Some(existing) = existing {
        let mut id = existing.id.clone();
        if enrolled.is_some() && (existing.refresh != refresh || refresh.is_none() || existing.api_key != candidate_api_key) {
            let Ok(mut replacements) = parse_credentials_with_options(&existing.name, &raw, true, Some(Lane::Google), candidate_api_key)
            else {
                return;
            };
            let Some(mut replacement) = replacements.pop().filter(|a| a.id == existing.id) else { return };
            // An access-only reread of the same token must not erase an enriched refresh token.
            if (replacement.refresh.is_some() && existing.refresh != replacement.refresh)
                || existing.access != replacement.access
                || existing.api_key != replacement.api_key
                || existing.base != replacement.base
            {
                let conflicted_existing: Vec<Account> = pool
                    .accounts
                    .values()
                    .filter(|other| other.api_key != replacement.api_key && other.shares_credential_fingerprint(&replacement))
                    .cloned()
                    .collect();
                let duplicate = pool.accounts.values().find(|other| other.id != id && other.shares_discovery_token(&replacement));
                let survivor_id = duplicate.filter(|other| other.compatible_refresh(&replacement)).map(|other| other.id.clone());
                let has_duplicate = duplicate.is_some();
                let enrollment = existing.provenance_enrollment.clone();
                let next_probe_gen = existing.probe_generation.saturating_add(1);
                if !conflicted_existing.is_empty() {
                    for other in &conflicted_existing {
                        pool.record_api_key_conflict(other);
                    }
                    pool.record_api_key_conflict(&replacement);
                }
                pool.revision += 1;
                if let Some(duplicate_id) = survivor_id {
                    // Keep the account that already holds the credential and its route evidence.
                    pool.accounts.remove(&id);
                    id = duplicate_id;
                    if let Some(survivor) = pool.accounts.get_mut(&id) {
                        merge_google_duplicate(survivor, replacement, client.is_some(), secret.is_some());
                    }
                } else {
                    if has_duplicate {
                        replacement.discard_discovered_access();
                    }
                    replacement.provenance_enrollment = enrollment;
                    replacement.probe_generation = next_probe_gen;
                    pool.insert(vec![replacement]);
                    return;
                }
            }
        }
        let Some(existing) = pool.accounts.get_mut(&id) else { return };
        if existing.api_key || candidate_api_key {
            return;
        }
        let relabel = existing.local
            && existing.provenance_enrollment.as_deref().is_none_or(|enrollment| enrollment == label)
            && existing.label != label
            && (existing.label == existing.name || existing.label.starts_with("local-") || existing.label.len() == 64);
        // An access-only reread must not fill in stale credentials for an adopted refresh token.
        let same_refresh = existing.refresh == refresh;
        let enriched = (same_refresh
            && ((existing.google_client.is_none() && client.is_some()) || (existing.google_secret.is_none() && secret.is_some())))
            || (existing.email.is_none() && email.is_some());
        if relabel {
            existing.label = label.to_owned();
        }
        if same_refresh && existing.google_client.is_none() {
            existing.google_client = client;
        }
        if same_refresh && existing.google_secret.is_none() {
            existing.google_secret = secret;
        }
        if existing.local && existing.provenance_enrollment.is_none() {
            existing.provenance_enrollment = Some(label.to_owned());
        }
        let project_changed = (existing.provenance_enrollment.as_deref() == Some(label)
            || existing.provenance_project.as_deref() == Some(label)
            || existing.project.is_none())
            && project.is_some()
            && existing.set_google_project(project);
        if project_changed {
            existing.provenance_project = Some(label.to_owned());
        }
        if existing.email.is_none() {
            existing.email = email;
        }
        existing.scrub_label(&[]);
        // Track the last discovered token separately from any access token the
        // proxy refreshed in memory; an unchanged disk snapshot is not a renewal.
        let mut rotated = false;
        if existing.local
            && same_refresh
            && existing.provenance_enrollment.as_deref() == Some(label)
            && let Some(access) = text(&raw, &["access_token", "accessToken", "access", "token"])
                .filter(|s| s.len() <= 32_768 && s.bytes().all(|b| b.is_ascii_graphic()))
        {
            let digest = Sha256::digest(access).into();
            let expiry = ["expires_at", "expiresAt", "expiry", "expires"].into_iter().find_map(|key| date(&raw[key]));
            let previous = existing.provenance_token_fp.replace(digest);
            let expired = existing.access.is_empty() || existing.expires.is_some_and(|at| at <= Utc::now());
            if (previous.is_some_and(|fp| fp != digest) || (previous.is_none() && expired))
                && access != existing.access
                && expiry.is_none_or(|at| at > Utc::now())
            {
                existing.replace_access(access.to_owned());
                existing.expires = expiry;
                rotated = true;
            }
        }
        if rotated {
            existing.clear_account_error();
            existing.probe_failed = false;
            existing.probe_generation = existing.probe_generation.saturating_add(1);
            existing.error_generation = existing.error_generation.saturating_add(1);
        }
        pool.revision += i64::from(relabel || enriched || project_changed || rotated);
        if rotated && let Some(rotated_account) = pool.accounts.get(&id).cloned() {
            let conflicted: Vec<Account> = pool
                .accounts
                .values()
                .filter(|other| {
                    other.id != id && other.api_key != rotated_account.api_key && other.shares_credential_fingerprint(&rotated_account)
                })
                .cloned()
                .collect();
            if !conflicted.is_empty() {
                for other in &conflicted {
                    pool.record_api_key_conflict(other);
                }
                pool.record_api_key_conflict(&rotated_account);
            }
            pool.propagate_api_key_conflicts();
            let duplicate =
                pool.accounts.values().find(|other| other.id != id && other.lane == Lane::Google && other.access == rotated_account.access);
            if let Some(other) = duplicate {
                if other.compatible_refresh(&rotated_account) {
                    let duplicate_id = other.id.clone();
                    if let Some(replacement) = pool.accounts.remove(&id)
                        && let Some(survivor) = pool.accounts.get_mut(&duplicate_id)
                    {
                        // Keep the surviving route's OAuth client unless it needs to adopt the refresh token.
                        merge_google_duplicate(survivor, replacement, false, false);
                    }
                } else if let Some(account) = pool.accounts.get_mut(&id) {
                    // Use this enrollment's own refresh token instead of retrying the shared access token.
                    account.replace_access(String::new());
                    account.expires = None;
                }
            }
        }
        pool.propagate_api_key_conflicts();
        return;
    }
    let source = format!("provenance:{label}");
    add_with_api_key(pool, Lane::Google, &source, raw, candidate_api_key);
    if let Some(account) = pool.accounts.get_mut(&local_credential_name(Lane::Google, &source)) {
        account.provenance_enrollment = Some(label.to_owned());
        if !account.access.is_empty() {
            account.provenance_token_fp = Some(Sha256::digest(&account.access).into());
        }
    }
}

fn object_api_key_evidence(record: &Value, lane: Lane) -> (bool, bool) {
    let api_wrapper = record.get("api_key").is_some_and(Value::is_object) || record.get("api-key").is_some_and(Value::is_object);
    let has_keys_array = record.get("keys").is_some_and(Value::is_array);
    let bool_api = record["api_key"].as_bool() == Some(true) || record["api-key"].as_bool() == Some(true);
    let key = record.as_str().or_else(|| text(record, &["api_key", "api-key", "key", "OPENAI_API_KEY"]));
    let access = key.or_else(|| text(record, &["access_token", "accessToken", "access", "token"])).unwrap_or_default();
    let refresh = text(record, &["refresh_token", "refreshToken", "refresh"]);
    // Inspect every type/provider/auth marker independently so field order never hides an API-key marker.
    let markers: Vec<String> = ["type", "provider", "auth_type", "auth_mode"]
        .into_iter()
        .filter_map(|field| record.get(field).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_ascii_lowercase))
        .collect();
    let explicit_oauth =
        refresh.is_some() || markers.iter().any(|kind| matches!(kind.as_str(), "oauth" | "oauth2" | "chatgpt" | "chatgptauthtokens"));
    let is_api_key = lane != Lane::Go
        && (api_wrapper
            || has_keys_array
            || bool_api
            || key.is_some()
            || markers.iter().any(|kind| matches!(kind.as_str(), "api" | "apikey" | "api_key" | "api-key"))
            || (lane == Lane::Codex && access.starts_with("sk-"))
            || (lane == Lane::Google && access.starts_with("AIza")));
    (is_api_key, explicit_oauth)
}

fn record_api_key_evidence(record: &Value, lane: Lane) -> (bool, bool) {
    let mut is_api_key = false;
    let mut explicit_oauth = false;
    for candidate in [Some(record), record.get("tokens"), record.get("claudeAiOauth")].into_iter().flatten() {
        let (api, oauth) = object_api_key_evidence(candidate, lane);
        is_api_key |= api;
        explicit_oauth |= oauth;
    }
    (is_api_key, explicit_oauth)
}

fn collect_api_key_token_fps(value: &Value, lane: Lane, force_api_key: bool, depth: usize, out: &mut Vec<(Lane, [u8; 32])>) {
    if lane == Lane::Go || depth > 4 {
        return;
    }
    let mut push_token = |token: &str| {
        if !token.is_empty() && token.len() <= 32_768 && token.bytes().all(|b| b.is_ascii_graphic()) {
            out.push((lane, Sha256::digest(token).into()));
        }
    };
    if let Some(s) = value.as_str().filter(|s| !s.trim().is_empty()) {
        push_token(s);
        return;
    }
    let Some(obj) = value.as_object() else {
        return;
    };
    for field in ["api_key", "api-key", "key", "OPENAI_API_KEY"] {
        if let Some(val) = obj.get(field).and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
            push_token(val);
        }
    }
    let (is_api, _) = object_api_key_evidence(value, lane);
    let treat_as_api = force_api_key || is_api;
    if treat_as_api {
        for field in ["access_token", "accessToken", "access", "token", "refresh_token", "refreshToken", "refresh"] {
            if let Some(val) = obj.get(field).and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
                push_token(val);
            }
        }
    }
    for nested_key in ["tokens", "claudeAiOauth"] {
        if let Some(nested) = obj.get(nested_key) {
            collect_api_key_token_fps(nested, lane, treat_as_api, depth + 1, out);
        }
    }
    for wrapper_key in ["api_key", "api-key"] {
        if let Some(wrapper) = obj.get(wrapper_key).filter(|v| v.is_object()) {
            collect_api_key_token_fps(wrapper, lane, true, depth + 1, out);
        }
    }
    if let Some(keys) = obj.get("keys").and_then(Value::as_array) {
        for item in keys {
            collect_api_key_token_fps(item, lane, true, depth + 1, out);
        }
    }
}

fn extract_api_key_token_fps(record: &Value, lane: Lane, force_api_key: bool) -> Vec<(Lane, [u8; 32])> {
    let mut fps = Vec::new();
    collect_api_key_token_fps(record, lane, force_api_key, 0, &mut fps);
    fps
}

pub(crate) fn add_opencode(pool: &mut Pool, raw: &Value, excluded: &BTreeSet<Lane>) {
    let rows = raw.get("credentials").unwrap_or(raw);
    let object = rows.as_object().into_iter().flatten().map(|(name, row)| (name.as_str(), row));
    let list = rows.as_array().into_iter().flatten().filter_map(|row| Some((text(row, &["provider", "providerID", "id"])?, row)));
    for (name, entry) in object.chain(list) {
        if let Some(lane) = Lane::parse(name).filter(|lane| !excluded.contains(lane)) {
            if let Some(auth) = entry.get("auth") {
                // Classify outer and inner records independently before unwrapping so
                // neither layer can mask API-key markers or contradictory OAuth fields.
                let (outer_api, outer_oauth) = record_api_key_evidence(entry, lane);
                let (inner_api, inner_oauth) = record_api_key_evidence(auth, lane);
                let force_api_key = outer_api || inner_api;
                // Preserve every explicit API-key digest from both outer and inner layers
                // before unwrapping or rejecting the row.
                let mut explicit_fps = extract_api_key_token_fps(entry, lane, force_api_key);
                explicit_fps.extend(extract_api_key_token_fps(auth, lane, force_api_key));
                pool.merge_api_key_conflict_fps(&explicit_fps);
                if force_api_key && (outer_oauth || inner_oauth) {
                    continue;
                }
                add_with_api_key(pool, lane, name, auth.clone(), force_api_key);
            } else {
                let explicit_fps = extract_api_key_token_fps(entry, lane, false);
                pool.merge_api_key_conflict_fps(&explicit_fps);
                add(pool, lane, name, entry.clone());
            }
        }
    }
}

fn discover_claude(config: &Config, pool: &mut Pool) {
    let home = &config.home;
    let user = env::var("USER").unwrap_or_default();
    let mut claude = paths(&Path::new("/run/claude-code").join(&user));
    claude.push(Path::new("/run/claude-code").join(&user));
    claude.push(env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| home.join(".claude")).join(".credentials.json"));
    if let Some(raw) = read_json(&home.join(".config/meridian-opencode/profiles.json")) {
        let profiles = raw.get("profiles").unwrap_or(&raw);
        for (id, row) in profiles
            .as_object()
            .into_iter()
            .flatten()
            .map(|(id, row)| (id.as_str(), row))
            .chain(profiles.as_array().into_iter().flatten().map(|row| (text(row, &["id", "name"]).unwrap_or("profile"), row)))
        {
            add(pool, Lane::Claude, id, row.clone());
            if let Some(path) = text(row, &["credentialsFile", "credentialsPath", "tokenFile", "configDir"]) {
                let mut path = home.join(path);
                if path.is_dir() {
                    path = path.join(".credentials.json");
                }
                if path.starts_with(home) || path.starts_with(Path::new("/run/claude-code").join(&user)) {
                    claude.push(path);
                }
            }
        }
    }
    for path in claude {
        if let Some(raw) = read_json(&path).or_else(|| {
            if !path.starts_with(Path::new("/run/claude-code").join(&user))
                || !fs::metadata(&path).is_ok_and(|m| m.is_file() && m.len() <= 32_768)
            {
                return None;
            }
            let token = fs::read_to_string(&path).ok()?;
            Some(json!({"access_token":token.trim()}))
        }) {
            add(pool, Lane::Claude, &path.to_string_lossy(), raw);
        }
    }
}

fn discover_codex(config: &Config, pool: &mut Pool) {
    let codex = env::var_os("CODEX_HOME").map(PathBuf::from).unwrap_or_else(|| config.home.join(".codex")).join("auth.json");
    if let Some(raw) = read_json(&codex).filter(|v| text(v, &["/tokens/refresh_token"]) != Some("spillway-pool")) {
        add(pool, Lane::Codex, "codex", raw);
    }
}

fn discover_opencode(config: &Config, pool: &mut Pool) {
    let opencode =
        env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| config.home.join(".local/share")).join("opencode/auth.json");
    if let Some(raw) =
        env::var("OPENCODE_AUTH_CONTENT").ok().and_then(|raw| serde_json::from_str(&raw).ok()).or_else(|| read_json(&opencode))
    {
        add_opencode(pool, &raw, &config.exclude_local_lanes);
    }
}

fn discover(config: &Config, pool: &mut Pool) {
    let excluded = &config.exclude_local_lanes;
    if !excluded.contains(&Lane::Claude) {
        discover_claude(config, pool);
    }
    if !excluded.contains(&Lane::Codex) {
        discover_codex(config, pool);
    }
    discover_opencode(config, pool);
    for (var, lane) in [
        ("OPENCODE_GO_API_KEY", Lane::Go),
        ("OPENCODE_API_KEY", Lane::Go),
        ("OPENAI_API_KEY", Lane::Codex),
        ("ANTHROPIC_API_KEY", Lane::Claude),
    ] {
        if !excluded.contains(&lane)
            && let Ok(key) = env::var(var)
        {
            add(pool, lane, var, json!({"key":key}));
        }
    }
    if excluded.contains(&Lane::Google) {
        return;
    }
    let user = env::var("USER").unwrap_or_default();
    let manifests: Vec<_> = paths(&config.provenance_oauth_dir)
        .into_iter()
        .filter(|path| {
            !user.is_empty()
                && path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(&format!("{user}-")) && n.ends_with(".json"))
        })
        .filter_map(|path| Some((path.file_stem()?.to_str()?.to_owned(), read_json(&path)?)))
        .collect();
    let ag = config.t3code_home.join("userdata/providers/antigravity");
    for profile in paths(&ag) {
        let Some(id) = profile.file_name().and_then(|n| n.to_str()) else { continue };
        if let Some(mut raw) = read_json(&profile.join("antigravity-acp/acp_token.json")) {
            if let Some((name, metadata)) = manifests.iter().find(|(name, data)| {
                name.strip_prefix(&format!("{user}-")) == Some(id)
                    || (text(data, &["/target/provider"]) == Some("google")
                        && text(data, &["/target/profile"]) == Some("antigravity")
                        && text(data, &["/target/account", "/target/enrollment"]) == Some(id))
            }) && let Some(fields) = raw.as_object_mut()
            {
                for field in ["email", "client_id", "client_secret", "project_id", "label"] {
                    if let Some(v) = metadata.get(field) {
                        fields.entry(field).or_insert_with(|| v.clone());
                    }
                }
                fields.entry("label").or_insert_with(|| json!(text(metadata, &["/target/account", "/target/enrollment"]).unwrap_or(name)));
            }
            add(pool, Lane::Google, id, raw);
        }
    }
}
