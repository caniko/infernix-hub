use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    env, fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use contracts::providers::*;
use serde::{Deserialize, Serialize};
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

#[derive(Clone, Copy)]
enum QuotaTarget<'a> {
    Model(&'a str),
    Bucket(Lane, &'a str),
    GoogleGemini,
}

fn is_google_gemini_model(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model.starts_with("gemini-") && !model.contains("argon")
}

#[cfg(test)]
pub(crate) fn window_applies_to_model(w: &QuotaWindow, model: &str) -> bool {
    window_applies(w, QuotaTarget::Model(model))
}

fn window_applies(w: &QuotaWindow, target: QuotaTarget<'_>) -> bool {
    if w.kind == QuotaWindowKind::Spend || w.unit == QuotaUnit::Credits {
        return false;
    }
    if let QuotaTarget::Bucket(lane, scope) = target {
        return w
            .scope_model
            .as_deref()
            .is_none_or(|s| if lane == Lane::Google { google_bucket(s) == scope } else { s.eq_ignore_ascii_case(scope) });
    }
    let QuotaTarget::Model(model) = target else {
        let key = w.window_key.to_ascii_lowercase();
        return !key.starts_with("gemini-argon")
            && !key.starts_with("3p-")
            && (key.starts_with("gemini-") || w.scope_model.as_deref().is_none_or(|scope| scope.eq_ignore_ascii_case("gemini")));
    };
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
        "3p" | "claude & gpt (shared)" | "claude-gpt" => claude || gpt,
        "claude" => claude,
        "gpt" => gpt,
        "opus" | "claude opus" => model.contains("opus"),
        "sonnet" | "claude sonnet" => model.contains("sonnet"),
        "haiku" | "claude haiku" => model.contains("haiku"),
        _ => model == scope || model.starts_with(&format!("{scope}-")) || (!scope.contains(' ') && model.contains(&scope)),
    }
}

fn google_bucket(scope: &str) -> String {
    let scope = scope.to_ascii_lowercase();
    match scope.as_str() {
        _ if scope.starts_with("gemini") && scope.contains("argon") => "gemini-argon".into(),
        _ if scope.starts_with("gemini") => "gemini".into(),
        _ if scope.starts_with("claude") || scope.starts_with("gpt") => "3p".into(),
        _ => scope,
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
        && (w.status == QuotaStatus::Rejected
            || (w.unit == QuotaUnit::Percent && effective_remaining_percent_at(w, &[], Utc::now()) == Some(0.0))
            || w.used_percent.is_some_and(|n| n >= 100.0))
}

pub(crate) fn quota_window_active(lane: Lane, w: &QuotaWindow, now: DateTime<Utc>) -> bool {
    w.resets_at
        .map_or_else(|| lane != Lane::Google || w.observed_at.is_some_and(|at| now - at < chrono::TimeDelta::hours(1)), |at| at > now)
}

pub(crate) const MAX_COOLDOWN_SECS: i64 = 604_800;

pub(crate) fn bounded_identifier(model: &str) -> bool {
    !model.is_empty() && model.len() <= 192 && model.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-:/".contains(&b))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
        self.reader.strong_count() != 0 || self.unverified_until.max(self.provisional.map(|c| c.until)).is_some_and(|until| until > now)
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
    pub(super) auth_plan: Option<String>,
    pub(super) auth_multiplier: Option<f64>,
    pub(super) observed_plan: Option<String>,
    pub access: String,
    pub refresh: Option<String>,
    // In-memory only: labels on disk can still contain retired credentials.
    // Keep safe labels keyed by the original label digest for the life of this account.
    redacted_labels: HashMap<[u8; 32], String>,
    pub expires: Option<DateTime<Utc>>,
    pub chatgpt_id: Option<String>,
    pub chatgpt_user: Option<String>,
    pub organization: Option<String>,
    pub codex_client_id: Option<String>,
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

    pub(crate) fn compute_source_fingerprint(&self, id_token: Option<&str>) -> String {
        let material = json!([
            self.access,
            self.refresh,
            self.codex_oauth_explicit,
            self.expires,
            id_token,
            self.google_client,
            self.google_secret,
            self.chatgpt_id,
            self.chatgpt_user,
            self.organization,
            self.project,
        ]);
        hex::encode(Sha256::digest(material.to_string()))
    }

    pub(crate) fn same_secret(&self, other: &Self) -> bool {
        self.same_credential(other)
            && self.access == other.access
            && self.refresh == other.refresh
            && self.google_client == other.google_client
            && self.google_secret == other.google_secret
    }

    pub(crate) fn stable_credential_fingerprint(&self) -> String {
        let identity = if self.api_key {
            // This field holds the static key for API-key accounts, not an OAuth access token.
            ("api_key", Some(self.access.as_str()))
        } else if let Some(enrollment) = &self.provenance_enrollment {
            ("enrollment", Some(enrollment.as_str()))
        } else if let Some(refresh) = &self.refresh {
            ("refresh_token", Some(refresh.as_str()))
        } else {
            ("account", self.chatgpt_user.as_deref().or(self.chatgpt_id.as_deref()).or(self.email.as_deref()))
        };
        let material = json!([self.lane.name(), self.auth_index, self.provider, identity, self.project]);
        hex::encode(Sha256::digest(material.to_string()))
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

    fn transient_recovery(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let error = self.active_quarantine(now)?;
        error.kind.is_transient().then_some(error.backoff_until).flatten().filter(|until| *until > now)
    }

    fn quota_backoff(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.active_quarantine(now).filter(|e| e.kind == AccountErrorKind::QuotaExhausted)?.backoff_until.filter(|until| *until > now)
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

    fn scoped_cooldown(&self, target: QuotaTarget<'_>) -> Option<Cooldown> {
        let scope = self.target_cooldown_scope(target)?;
        self.quota_cooldown(Some(&scope))
    }

    fn target_cooldown_scope(&self, target: QuotaTarget<'_>) -> Option<String> {
        match target {
            QuotaTarget::Model(model) => model_cooldown_scope(self.lane, model),
            QuotaTarget::GoogleGemini => Some("gemini".into()),
            QuotaTarget::Bucket(Lane::Google, scope) => Some(scope.to_owned()),
            QuotaTarget::Bucket(..) => None,
        }
    }

    fn quota_cooldown(&self, scope: Option<&str>) -> Option<Cooldown> {
        let matches = |saved| self.same_cooldown_scope(saved, scope);
        let settled = scope.map_or(self.cooldown, |_| {
            self.scoped_cooldowns.iter().filter(|(key, _)| matches(Some(key))).map(|(_, cooldown)| *cooldown).reduce(Cooldown::merge)
        });
        settled
            .into_iter()
            .chain(self.pending_quota.iter().filter(|pending| matches(pending.scope.as_deref())).filter_map(|pending| pending.provisional))
            .reduce(Cooldown::merge)
    }

    fn same_cooldown_scope(&self, saved: Option<&str>, scope: Option<&str>) -> bool {
        saved == scope || (self.lane == Lane::Google && saved.zip(scope).is_some_and(|(saved, scope)| google_bucket(saved) == scope))
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

    #[cfg(test)]
    pub(super) fn has_unverified_google_quota_read(&self, model: &str, now: DateTime<Utc>) -> bool {
        self.has_unverified_quota_read(QuotaTarget::Model(model), now)
    }

    fn has_unverified_quota_read(&self, target: QuotaTarget<'_>, now: DateTime<Utc>) -> bool {
        let scope = self.target_cooldown_scope(target);
        // An unread or failed body still blocks verification until its own backoff expires,
        // even if a longer cooldown wins the legacy routing decision.
        self.pending_quota.iter().any(|pending| {
            (pending.scope.is_none() || self.same_cooldown_scope(pending.scope.as_deref(), scope.as_deref()))
                && pending.blocks_verification(now)
        })
    }

    fn active_quota_windows<'a>(&'a self, now: DateTime<Utc>, target: QuotaTarget<'a>) -> impl Iterator<Item = &'a QuotaWindow> {
        self.windows.iter().filter(move |w| {
            window_applies(w, target)
                && quota_window_active(self.lane, w, now)
                && (enforces_quota(w)
                    || (w.unit == QuotaUnit::Percent
                        && self.effective_percent(w, now) == Some(0.0)
                        // An inherited limit has the longer window's deadline, not the short window's.
                        && !self.windows.iter().any(|longer| {
                            window_binds(longer, w)
                                && window_applies(longer, target)
                                && quota_window_active(self.lane, longer, now)
                                && enforces_quota(longer)
                        })))
        })
    }

    fn effective_percent(&self, window: &QuotaWindow, now: DateTime<Utc>) -> Option<f64> {
        if self.lane == Lane::Google
            && self.windows.iter().any(|w| w.resets_at.is_none() && window_binds(w, window) && !quota_window_active(self.lane, w, now))
        {
            // Google also ages out resetless evidence on a local TTL.
            let active = self.windows.iter().filter(|w| quota_window_active(self.lane, w, now)).cloned().collect::<Vec<_>>();
            effective_remaining_percent_at(window, &active, now)
        } else {
            effective_remaining_percent_at(window, &self.windows, now)
        }
    }

    fn capacity_multiplier(&self) -> f64 {
        self.multiplier.filter(|m| m.is_finite() && *m > 0.0).unwrap_or(1.0)
    }

    fn percent_windows<'a>(&'a self, target: QuotaTarget<'a>) -> impl Iterator<Item = &'a QuotaWindow> {
        self.windows.iter().filter(move |w| w.unit == QuotaUnit::Percent && w.kind != QuotaWindowKind::Spend && window_applies(w, target))
    }

    fn capacity_score(&self, now: DateTime<Utc>, target: QuotaTarget<'_>, in_flight: usize) -> CapacityScore {
        let idle_clock = |w: &QuotaWindow| {
            w.resets_at.is_none()
                && w.observation_state == QuotaObservationState::Measured
                && !w.stale
                && w.remaining_percent.map_or_else(|| w.used_percent.is_some_and(|u| u <= 0.0), |r| r >= 100.0)
        };
        let windows = || {
            self.percent_windows(target)
                .filter(|w| quota_window_active(self.lane, w, now) || idle_clock(w))
                .filter_map(|w| Some((w, self.effective_percent(w, now)? / 100.0, infer_window_duration_mins(w))))
        };
        let tie = |a: &(&QuotaWindow, f64, Option<u32>), b: &(&QuotaWindow, f64, Option<u32>)| {
            b.1.total_cmp(&a.1).then_with(|| b.0.window_key.cmp(&a.0.window_key))
        };
        let is_rolling =
            |(w, _, mins): &(&QuotaWindow, f64, Option<u32>)| w.kind == QuotaWindowKind::Session || mins.is_some_and(|m| m <= 1440);
        let session = windows().max_by(|a, b| {
            is_rolling(a).cmp(&is_rolling(b)).then_with(|| b.2.unwrap_or(u32::MAX).cmp(&a.2.unwrap_or(u32::MAX))).then_with(|| tie(a, b))
        });
        let weekly = windows().filter(|(w, _, mins)| w.kind == QuotaWindowKind::Weekly || mins.is_some_and(|m| m > 1440)).max_by(|a, b| {
            let rank = |(w, _, mins): &(&QuotaWindow, f64, Option<u32>)| mins.map(|m| (w.kind == QuotaWindowKind::Weekly || m <= 10080, m));
            rank(a).cmp(&rank(b)).then_with(|| tie(a, b))
        });
        let hours = |reset: DateTime<Utc>| ((reset - now).num_seconds().max(0) as f64 / 3600.0).max(1.0);
        // With zero measured buckets keep routing available, but do not rank unknown headroom as full.
        let session_left = session.map(|(_, left, _)| left);
        let rolling = session.filter(is_rolling).map(|(w, _, _)| w);
        let multiplier = self.capacity_multiplier();
        let rate = weekly.or(session).map(|(w, left, mins)| {
            let hours = w.resets_at.map_or((f64::from(mins.unwrap_or(300)) / 60.0).max(1.0), hours);
            (multiplier * left / hours) / (1.0 + in_flight as f64)
        });
        CapacityScore {
            session_left,
            session_capacity: multiplier * session_left.unwrap_or(0.0),
            rate,
            multiplier,
            in_flight,
            active: rolling.is_none_or(|w| w.resets_at.is_some_and(|r| r > now) || w.used_percent.is_some_and(|u| u > 0.0)),
            idle: rolling.is_some_and(idle_clock),
        }
    }

    pub fn quota_limited_until(&self, now: DateTime<Utc>, model: &str) -> Option<DateTime<Utc>> {
        self.quota_limited_until_for(now, QuotaTarget::Model(model))
    }

    fn quota_limited_until_for(&self, now: DateTime<Utc>, target: QuotaTarget<'_>) -> Option<DateTime<Utc>> {
        self.active_quota_windows(now, target)
            .map(|w| self.window_recovery(w).unwrap_or(DateTime::<Utc>::MAX_UTC))
            .chain(self.quota_cooldown(None).into_iter().chain(self.scoped_cooldown(target)).map(|c| c.until).filter(|until| *until > now))
            .chain(self.quota_backoff(now))
            .max()
    }

    fn window_recovery(&self, w: &QuotaWindow) -> Option<DateTime<Utc>> {
        w.resets_at
            .or_else(|| (self.lane == Lane::Google).then_some(w.observed_at).flatten()?.checked_add_signed(chrono::TimeDelta::hours(1)))
    }

    fn bounded_recovery(&self, now: DateTime<Utc>, target: QuotaTarget<'_>) -> Option<DateTime<Utc>> {
        let quarantine = if self.active_quarantine(now).is_some() {
            Some(self.transient_recovery(now).or_else(|| self.quota_backoff(now))?)
        } else {
            None
        };
        let window_reset =
            self.active_quota_windows(now, target).try_fold(None, |latest, w| Some(latest.max(Some(self.window_recovery(w)?))))?;
        let cooldown =
            self.quota_cooldown(None).into_iter().chain(self.scoped_cooldown(target)).map(|c| c.until).filter(|at| *at > now).max();
        window_reset.max(cooldown).max(quarantine).filter(|at| *at != DateTime::<Utc>::MAX_UTC)
    }

    fn eligibility_blockers(&self, now: DateTime<Utc>, target: QuotaTarget<'_>) -> (Vec<&'static str>, Option<DateTime<Utc>>) {
        let horizon = chrono::TimeDelta::seconds(MAX_COOLDOWN_SECS);
        let mut reasons = Vec::new();
        let mut latest = Some(now);
        let mut block = |reason, at: Option<DateTime<Utc>>| {
            reasons.push(reason);
            latest = latest.zip(at.filter(|at| *at > now && *at - now <= horizon)).map(|(old, at)| old.max(at));
        };
        if let Some(error) = self.active_quarantine(now) {
            block(if error.kind == AccountErrorKind::AuthExpired { "auth_cooldown" } else { "quarantined" }, None);
        }
        for (reason, cooldown) in [("global_cooldown", self.quota_cooldown(None)), ("scoped_cooldown", self.scoped_cooldown(target))] {
            if let Some(c) = cooldown.filter(|c| c.until > now) {
                block(reason, c.verified_reset.then_some(c.until));
            }
        }
        let mut windows = self.active_quota_windows(now, target).peekable();
        if windows.peek().is_some() {
            // A cached reset must have been credible when measured, not just when read.
            let reset = windows.try_fold(now, |latest, w| {
                let (observed, at) = (w.observed_at?, w.resets_at?);
                (!w.stale
                    && w.observation_state == QuotaObservationState::Measured
                    && observed <= now
                    && observed < at
                    && at - observed <= horizon)
                    .then_some(latest.max(at))
            });
            block("quota_window", reset);
        }
        let verified = latest.filter(|_| !reasons.is_empty() && !self.has_unverified_quota_read(target, now));
        (reasons, verified)
    }

    fn google_preview_targets(&self) -> Vec<QuotaTarget<'_>> {
        if !self.models.is_empty() {
            return self.models.iter().map(String::as_str).filter(|m| is_google_gemini_model(m)).map(QuotaTarget::Model).collect();
        }
        let mut targets: Vec<QuotaTarget<'_>> = self
            .windows
            .iter()
            .filter_map(|w| w.scope_model.as_deref())
            .filter(|s| is_google_gemini_model(s))
            .map(QuotaTarget::Model)
            .collect();
        if targets.is_empty() || self.windows.iter().any(|w| window_applies(w, QuotaTarget::GoogleGemini)) {
            targets.push(QuotaTarget::GoogleGemini);
        }
        targets
    }

    fn eligible_quota_targets<'a>(&'a self, now: DateTime<Utc>, target: QuotaTarget<'a>) -> Vec<QuotaTarget<'a>> {
        match target {
            QuotaTarget::Model(_) | QuotaTarget::Bucket(..) => {
                self.quota_limited_until_for(now, target).is_none().then_some(target).into_iter().collect()
            }
            QuotaTarget::GoogleGemini => {
                self.google_preview_targets().into_iter().filter(|t| self.quota_limited_until_for(now, *t).is_none()).collect()
            }
        }
    }

    fn eligible_for_quota(&self, now: DateTime<Utc>, target: QuotaTarget<'_>) -> bool {
        if self.quarantined(now) {
            return false;
        }
        match target {
            QuotaTarget::Model(_) | QuotaTarget::Bucket(..) => self.quota_limited_until_for(now, target).is_none(),
            QuotaTarget::GoogleGemini => !self.eligible_quota_targets(now, target).is_empty(),
        }
    }

    fn reset_pair(&self, now: DateTime<Utc>, target: QuotaTarget<'_>) -> (DateTime<Utc>, DateTime<Utc>) {
        let reset = |applies: fn(&QuotaWindow) -> bool| {
            self.windows
                .iter()
                .filter(|w| applies(w) && window_applies(w, target))
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
        (weekly, session)
    }

    fn soonest_reset(&self, now: DateTime<Utc>, target: QuotaTarget<'_>) -> (DateTime<Utc>, DateTime<Utc>, std::cmp::Reverse<i32>) {
        let (weekly, session) = match target {
            QuotaTarget::Model(_) | QuotaTarget::Bucket(..) => self.reset_pair(now, target),
            QuotaTarget::GoogleGemini => self
                .eligible_quota_targets(now, target)
                .into_iter()
                .map(|t| self.reset_pair(now, t))
                .min()
                .unwrap_or((DateTime::<Utc>::MAX_UTC, DateTime::<Utc>::MAX_UTC)),
        };
        (weekly, session, std::cmp::Reverse(self.priority))
    }

    fn max_used_percent(&self, now: DateTime<Utc>, target: QuotaTarget<'_>) -> f64 {
        let score = |applies: &dyn Fn(&QuotaWindow) -> bool| {
            self.windows
                .iter()
                .filter(|w| matches!(w.kind, QuotaWindowKind::Weekly | QuotaWindowKind::Session | QuotaWindowKind::ModelScoped))
                .filter(|w| applies(w))
                .filter(|w| quota_window_active(self.lane, w, now))
                .filter_map(|w| w.used_percent)
                .fold(0.0_f64, f64::max)
        };
        match target {
            QuotaTarget::Model(_) | QuotaTarget::Bucket(..) => score(&|w| window_applies(w, target)),
            QuotaTarget::GoogleGemini => {
                let targets = self.eligible_quota_targets(now, target);
                score(&|w| targets.iter().any(|t| window_applies(w, *t)))
            }
        }
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
        });
        if self.plan.is_some() || self.multiplier.is_some() {
            let mut plan = serde_json::Map::new();
            if let Some(name) = &self.plan {
                for key in ["name", "tier", "label"] {
                    plan.insert(key.into(), json!(name));
                }
            }
            if let Some(multiplier) = self.multiplier {
                plan.insert("multiplier".into(), json!(multiplier));
            }
            row["plan"] = Value::Object(plan);
        }
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
        let chatgpt_id = text(
            entry,
            &[
                "account_id",
                "accountId",
                "chatgpt_account_id",
                "/tokens/account_id",
                "/id_token/chatgpt_account_id",
                "/metadata/accountID",
                "/metadata/account_id",
            ],
        )
        .or_else(|| text(access_chat, &["chatgpt_account_id"]))
        .or_else(|| text(chat, &["chatgpt_account_id"]))
        .map(str::to_owned);
        let chatgpt_user = text(access_chat, &["chatgpt_user_id"]).or_else(|| text(chat, &["chatgpt_user_id"])).map(str::to_owned);
        let organization = text(entry, &["organization_uuid", "organizationUuid", "/claudeAiOauth/organizationUuid"]).map(str::to_owned);
        let codex_client_id = (lane == Lane::Codex)
            .then(|| {
                text(entry, &["client_id", "clientID", "/metadata/clientID", "/metadata/client_id"])
                    .or_else(|| text(raw, &["client_id", "clientID", "/metadata/clientID", "/metadata/client_id"]))
                    .map(str::to_owned)
            })
            .flatten();
        let auth_plan = [entry, group, raw]
            .into_iter()
            .find_map(|record| {
                text(
                    record,
                    &[
                        "plan",
                        "/plan/tier",
                        "plan_type",
                        "/id_token/plan_type",
                        "/id_token/chatgpt_plan_type",
                        "/claudeAiOauth/subscriptionType",
                    ],
                )
            })
            .map(str::to_owned);
        let observed_plan = text(chat, &["chatgpt_plan_type"]).map(str::to_owned);
        let auth_multiplier = [entry, group, raw].into_iter().find_map(|record| {
            record["tier_multiplier"]
                .as_f64()
                .or_else(|| record.pointer("/plan/multiplier").and_then(Value::as_f64))
                .filter(|n| n.is_finite() && *n > 0.0)
        });
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
            email: text(entry, &["email"]).or_else(|| text(raw, &["email"])).or_else(|| text(&claims, &["email"])).map(str::to_owned),
            note,
            plan: auth_plan.clone().or_else(|| observed_plan.clone()),
            disabled,
            priority,
            weight,
            multiplier: auth_multiplier,
            auth_plan,
            auth_multiplier,
            observed_plan,
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
            codex_client_id,
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
        account.source_fingerprint = Some(account.compute_source_fingerprint(id_token));
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

#[derive(Deserialize, Serialize)]
pub(crate) struct CooldownState {
    version: u32,
    accounts: BTreeMap<String, AccountQuotaState>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    unmatched_accounts: Vec<(String, AccountQuotaState)>,
}

#[derive(Clone, Deserialize, Serialize)]
struct SavedQuotaSuppression {
    scope: Option<String>,
    provisional: Option<Cooldown>,
    unverified_until: Option<DateTime<Utc>>,
}

#[derive(Clone, Deserialize, Serialize)]
struct AccountQuotaState {
    route: String,
    lane: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    credential_fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    quota_backoff_until: Option<DateTime<Utc>>,
    cooldown: Option<Cooldown>,
    scoped_cooldowns: BTreeMap<String, Cooldown>,
    #[serde(default)]
    pending_quota: Vec<SavedQuotaSuppression>,
    windows: Vec<QuotaWindow>,
    observed: Option<DateTime<Utc>>,
    grants: Vec<ResetGrant>,
    google_fallback_keys: BTreeSet<String>,
}

impl AccountQuotaState {
    fn route(account: &Account) -> String {
        let route = json!([
            account.lane.name(),
            account.api_key,
            account.project,
            account.base.as_ref().map(|url| url.as_str()),
            account.chatgpt_id,
            account.chatgpt_user,
        ]);
        hex::encode(Sha256::digest(route.to_string()))
    }

    fn retain_current(&mut self, lane: Lane, now: DateTime<Utc>) {
        self.cooldown = self.cooldown.filter(|c| c.until > now);
        self.quota_backoff_until = self.quota_backoff_until.filter(|at| *at > now);
        let valid_scope = |scope: &str| {
            if lane == Lane::Google { bounded_identifier(&google_bucket(scope)) } else { bounded_identifier(scope) }
        };
        self.scoped_cooldowns.retain(|scope, c| valid_scope(scope) && c.until > now);
        let horizon = chrono::TimeDelta::seconds(MAX_COOLDOWN_SECS);
        self.pending_quota.retain_mut(|pending| {
            let valid = |at: DateTime<Utc>| at > now && at - now <= horizon;
            pending.provisional = pending.provisional.filter(|c| valid(c.until)).map(|c| Cooldown { verified_reset: false, ..c });
            pending.unverified_until = pending.unverified_until.filter(|at| valid(*at));
            lane == Lane::Google
                && pending.scope.as_deref().is_none_or(valid_scope)
                && (pending.provisional.is_some() || pending.unverified_until.is_some())
        });
        self.windows.retain(|w| {
            !w.window_key.is_empty()
                && w.observed_at.is_none_or(|at| at <= now)
                && (w.observed_at.is_some() || w.resets_at.is_some())
                && (matches!(w.observation_state, QuotaObservationState::Measured | QuotaObservationState::StaleLastGood)
                    || enforces_quota(w)
                    || w.used_percent.is_some()
                    || w.remaining_percent.is_some())
                && quota_window_active(lane, w, now)
        });
        self.observed = self.observed.filter(|at| *at <= now);
        self.grants.retain(|g| !g.grant_id.is_empty() && g.checked_at <= now && g.expires_at.is_none_or(|at| at > now));
        self.google_fallback_keys.retain(|key| self.windows.iter().any(|w| &w.window_key == key));
    }

    fn has_evidence(&self) -> bool {
        self.cooldown.is_some()
            || self.quota_backoff_until.is_some()
            || !self.scoped_cooldowns.is_empty()
            || !self.pending_quota.is_empty()
            || !self.windows.is_empty()
            || self.observed.is_some()
            || !self.grants.is_empty()
    }

    fn retain_unmatched(&mut self, now: DateTime<Utc>) -> bool {
        let Some(lane) = self.lane.as_deref().and_then(Lane::parse) else { return false };
        self.retain_current(lane, now);
        // The last observation's timestamp alone has no active evidence to restore.
        (self.credential_fingerprint.is_some() || self.source_fingerprint.is_some())
            && (self.cooldown.is_some()
                || self.quota_backoff_until.is_some()
                || !self.scoped_cooldowns.is_empty()
                || !self.pending_quota.is_empty()
                || !self.windows.is_empty()
                || !self.grants.is_empty())
    }
}

pub(crate) const COOLDOWN_STATE_FILE: &str = "cooldown-state.json";

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
        self.supports_for(pool, account, QuotaTarget::Model(self.model))
    }

    fn supports_for(&self, pool: &Pool, account: &Account, target: QuotaTarget<'_>) -> bool {
        self.allows_in(pool, account)
            && pool.supports(account, self.lane, self.model, self.required_project)
            && (!matches!(target, QuotaTarget::GoogleGemini)
                || account.models.is_empty()
                || account.models.iter().any(|m| is_google_gemini_model(m)))
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
    undiscovered_cooldowns: Vec<(String, AccountQuotaState)>,
    account_defaults: BTreeMap<String, Value>,
    account_overrides: BTreeMap<String, AccountOverride>,
    in_flight: Arc<Mutex<HashMap<String, usize>>>,
}

const AMPLE_SESSION_HEADROOM: f64 = 0.05;

struct CapacityScore {
    session_left: Option<f64>,
    session_capacity: f64,
    rate: Option<f64>,
    multiplier: f64,
    in_flight: usize,
    active: bool,
    idle: bool,
}

impl CapacityScore {
    fn ample(&self) -> bool {
        self.session_left.is_some_and(|left| left >= AMPLE_SESSION_HEADROOM)
    }
}

pub(crate) struct InFlightGuard {
    counts: Arc<Mutex<HashMap<String, usize>>>,
    id: String,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let mut counts = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        if let std::collections::hash_map::Entry::Occupied(mut entry) = counts.entry(std::mem::take(&mut self.id)) {
            *entry.get_mut() -= 1;
            if *entry.get() == 0 {
                entry.remove();
            }
        }
    }
}

const ACCOUNT_OVERRIDES_FILE: &str = "account-overrides.json";

#[derive(Default, Clone, Deserialize, Serialize)]
pub(crate) struct AccountOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "nullable")]
    pub note: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "nullable")]
    pub plan: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "nullable")]
    pub tier_multiplier: Option<Option<f64>>,
}

fn nullable<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<Option<T>>, D::Error> {
    Option::deserialize(d).map(Some)
}

impl AccountOverride {
    fn matches(&self, account: &Account) -> bool {
        self.credential_fingerprint.as_ref().is_some_and(|fp| *fp == account.stable_credential_fingerprint())
    }

    fn has_metadata(&self) -> bool {
        self.disabled.is_some() || self.priority.is_some() || self.weight.is_some() || self.note.is_some()
    }

    fn clear_metadata(&mut self) {
        self.disabled = None;
        self.priority = None;
        self.weight = None;
        self.note = None;
    }

    fn merge(&mut self, changes: Self) {
        self.disabled = changes.disabled.or(self.disabled);
        self.priority = changes.priority.or(self.priority);
        self.weight = changes.weight.or(self.weight);
        self.note = changes.note.or(self.note.take());
        self.plan = changes.plan.or(self.plan.take());
        self.tier_multiplier = changes.tier_multiplier.or(self.tier_multiplier);
    }

    fn apply(&self, account: &mut Account) {
        // Managed auth JSON is authoritative even if an older override still has duplicates.
        if account.local {
            account.disabled = self.disabled.unwrap_or(account.disabled);
            account.priority = self.priority.unwrap_or(account.priority);
            account.weight = self.weight.unwrap_or(account.weight);
            if let Some(note) = &self.note {
                account.note.clone_from(note);
            }
        }
        if let Some(plan) = &self.plan {
            account.plan.clone_from(plan);
        }
        if let Some(multiplier) = self.tier_multiplier {
            account.multiplier = multiplier.filter(|m| m.is_finite() && *m > 0.0);
        }
    }
}

fn apply_account_settings(defaults: &BTreeMap<String, Value>, overrides: &BTreeMap<String, AccountOverride>, account: &mut Account) {
    account.plan.clone_from(&account.auth_plan);
    account.multiplier = account.auth_multiplier;
    let stem = account.name.strip_suffix(".json").unwrap_or(&account.name);
    for key in [
        Some(account.id.as_str()),
        Some(stem),
        account.provenance_enrollment.as_deref(),
        Some(account.label.as_str()),
        account.email.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(entry) = defaults.get(&key.to_lowercase()) {
            if account.plan.is_none() {
                account.plan =
                    entry["plan"].as_str().map(str::trim).filter(|s| !s.is_empty() && s.chars().count() <= 128).map(str::to_owned);
            }
            if account.multiplier.is_none() {
                account.multiplier = entry["tier_multiplier"].as_f64().filter(|m| m.is_finite() && *m > 0.0);
            }
        }
    }
    if account.plan.is_none() {
        account.plan.clone_from(&account.observed_plan);
    }
    if let Some(settings) = overrides.get(&account.id).filter(|settings| settings.matches(account)) {
        settings.apply(account);
    }
}

impl Pool {
    pub(crate) fn acquire_in_flight(&self, id: &str) -> InFlightGuard {
        *self.in_flight.lock().unwrap_or_else(|e| e.into_inner()).entry(id.to_owned()).or_default() += 1;
        InFlightGuard { counts: self.in_flight.clone(), id: id.to_owned() }
    }

    pub(crate) fn apply_account_settings(&self, account: &mut Account) {
        apply_account_settings(&self.account_defaults, &self.account_overrides, account);
    }

    pub(crate) fn refresh_account_settings(&mut self, id: &str) -> Option<&Account> {
        let account = self.accounts.get_mut(id)?;
        apply_account_settings(&self.account_defaults, &self.account_overrides, account);
        Some(account)
    }

    fn commit_account_overrides(
        &mut self,
        state_dir: &Path,
        next: BTreeMap<String, AccountOverride>,
        credential_change: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let path = state_dir.join(ACCOUNT_OVERRIDES_FILE);
        let previous = serde_json::to_vec_pretty(&self.account_overrides)?;
        let updated = serde_json::to_vec_pretty(&next)?;
        if previous != updated {
            crate::config::write_private(&path, &updated, true)?;
        }
        if let Err(error) = credential_change() {
            if previous != updated
                && let Err(rollback) = crate::config::write_private(&path, &previous, true)
            {
                tracing::error!(%rollback, "could not roll back proxy account overrides");
            }
            return Err(error);
        }
        self.account_overrides = next;
        Ok(())
    }

    pub(crate) fn save_account_override(
        &mut self,
        state_dir: &Path,
        id: &str,
        mut changes: AccountOverride,
        clean_managed_metadata: bool,
    ) -> Result<()> {
        let account = self.accounts.get(id).ok_or_else(|| anyhow!("account disappeared during credential edit"))?;
        if !account.local {
            changes.clear_metadata();
        }
        let previous = self.account_overrides.get(id);
        if !changes.has_metadata()
            && changes.plan.is_none()
            && changes.tier_multiplier.is_none()
            && !(clean_managed_metadata && previous.is_some_and(AccountOverride::has_metadata))
        {
            return Ok(());
        }
        let mut updated = previous.filter(|settings| settings.matches(account)).cloned().unwrap_or_default();
        if !account.local {
            updated.clear_metadata();
        }
        updated.merge(changes);
        let mut next = self.account_overrides.clone();
        if updated.has_metadata() || updated.plan.is_some() || updated.tier_multiplier.is_some() {
            updated.credential_fingerprint = Some(account.stable_credential_fingerprint());
            next.insert(id.to_owned(), updated);
        } else {
            next.remove(id);
        }
        self.commit_account_overrides(state_dir, next, || Ok(()))?;
        self.refresh_account_settings(id);
        Ok(())
    }

    pub(crate) fn remove_account_overrides(
        &mut self,
        state_dir: &Path,
        ids: &[String],
        credential_change: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let mut next = self.account_overrides.clone();
        for id in ids {
            next.remove(id);
        }
        self.commit_account_overrides(state_dir, next, credential_change)?;
        self.undiscovered_cooldowns.retain(|(id, _)| !ids.contains(id));
        Ok(())
    }

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
        #[derive(Deserialize)]
        struct Defaults {
            version: u32,
            accounts: BTreeMap<String, Value>,
        }
        let account_defaults = config.account_defaults_file.as_deref().and_then(|path| {
            if let Some(file) = read_json(path).and_then(|raw| serde_json::from_value::<Defaults>(raw).ok()).filter(|f| f.version == 1) {
                return Some(file.accounts.into_iter().map(|(key, value)| (key.to_lowercase(), value)).collect());
            }
            tracing::warn!(path = %path.display(), "could not load proxy account defaults (expected version 1)");
            None
        });
        let path = config.state_dir.join(ACCOUNT_OVERRIDES_FILE);
        let account_overrides = match crate::config::read_text(&path) {
            Ok(None) => None,
            Ok(Some(raw)) => match serde_json::from_str(&raw) {
                Ok(settings) => Some(settings),
                Err(_) => {
                    tracing::warn!("ignoring malformed proxy account overrides");
                    None
                }
            },
            Err(error) => {
                tracing::warn!(%error, "could not read proxy account overrides");
                None
            }
        };
        let mut pool = Self {
            account_defaults: account_defaults.unwrap_or_default(),
            account_overrides: account_overrides.unwrap_or_default(),
            ..Self::default()
        };
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
        pool.restore_cooldown_state(&config.state_dir.join(COOLDOWN_STATE_FILE), Utc::now());
        Ok(pool)
    }

    fn restore_cooldown_state(&mut self, path: &Path, now: DateTime<Utc>) {
        let raw = match crate::config::read_text(path) {
            Ok(Some(raw)) => raw,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(%error, "could not read proxy cooldown state");
                return;
            }
        };
        let Ok(CooldownState { version: 1, accounts, unmatched_accounts }) = serde_json::from_str(&raw) else {
            tracing::warn!("ignoring corrupt or unsupported proxy cooldown state");
            return;
        };
        self.undiscovered_cooldowns = accounts.into_iter().chain(unmatched_accounts).collect();
        self.restore_discovered_cooldowns(now);
    }

    pub(crate) fn restore_discovered_cooldowns(&mut self, now: DateTime<Utc>) {
        self.undiscovered_cooldowns.retain_mut(|(id, saved)| {
            if !saved.retain_unmatched(now) {
                return false;
            }
            let Some(account) = self.accounts.get_mut(id) else { return true };
            let matches = saved.credential_fingerprint.as_ref().map_or_else(
                || saved.source_fingerprint.is_some() && account.source_fingerprint == saved.source_fingerprint,
                |fp| *fp == account.stable_credential_fingerprint(),
            );
            if !matches {
                return true;
            }
            if AccountQuotaState::route(account) != saved.route {
                return true;
            }
            if let Some(until) = saved.quota_backoff_until
                && account.active_quarantine(now).is_none_or(|err| err.kind == AccountErrorKind::QuotaExhausted)
                && account.quota_backoff(now).is_none_or(|current| until > current)
            {
                let mut error = crate::error_classify::local_error(
                    AccountErrorKind::QuotaExhausted,
                    AccountErrorSource::QuotaProbe,
                    "Quota probe backoff restored from disk.",
                );
                error.occurred_at = now;
                error.backoff_until = Some(until);
                account.last_error = Some(error);
            }
            if let Some(cooldown) = saved.cooldown {
                account.cooldown = Some(account.cooldown.map_or(cooldown, |old| old.merge(cooldown)));
            }
            for (scope, cooldown) in std::mem::take(&mut saved.scoped_cooldowns) {
                account.scoped_cooldowns.entry(scope).and_modify(|old| *old = old.merge(cooldown)).or_insert(cooldown);
            }
            account.pending_quota.extend(std::mem::take(&mut saved.pending_quota).into_iter().map(|p| PendingQuota {
                reader: Weak::new(),
                scope: p.scope,
                provisional: p.provisional,
                unverified_until: p.unverified_until,
            }));
            if account.observed.is_none() && account.windows.is_empty() {
                account.windows = std::mem::take(&mut saved.windows);
                account.observed = saved.observed;
                account.grants = std::mem::take(&mut saved.grants);
                account.google_fallback_keys = std::mem::take(&mut saved.google_fallback_keys);
            }
            false
        });
    }

    pub(crate) fn finish_startup_discovery(&mut self, now: DateTime<Utc>) {
        self.restore_discovered_cooldowns(now);
    }

    pub(crate) fn cooldown_state(&self, now: DateTime<Utc>) -> CooldownState {
        let mut accounts: BTreeMap<_, _> = self
            .accounts
            .values()
            .filter_map(|a| {
                let mut pending = BTreeMap::<_, SavedQuotaSuppression>::new();
                for p in &a.pending_quota {
                    let entry = pending.entry(p.scope.clone()).or_insert_with(|| SavedQuotaSuppression {
                        scope: p.scope.clone(),
                        provisional: None,
                        unverified_until: None,
                    });
                    if let Some(c) = p.provisional {
                        entry.provisional = Some(entry.provisional.map_or(c, |old| old.merge(c)));
                    }
                    entry.unverified_until = entry.unverified_until.max(p.unverified_until);
                }
                let mut saved = AccountQuotaState {
                    route: AccountQuotaState::route(a),
                    lane: Some(a.lane.name().into()),
                    source_fingerprint: None,
                    credential_fingerprint: Some(a.stable_credential_fingerprint()),
                    quota_backoff_until: a.quota_backoff(now),
                    cooldown: a.cooldown,
                    scoped_cooldowns: a.scoped_cooldowns.clone(),
                    pending_quota: pending.into_values().collect(),
                    windows: a.windows.clone(),
                    observed: a.observed,
                    grants: a.grants.clone(),
                    google_fallback_keys: a.google_fallback_keys.clone(),
                };
                saved.retain_current(a.lane, now);
                saved.has_evidence().then(|| (a.id.clone(), saved))
            })
            .collect();
        let mut unmatched_accounts = Vec::new();
        for (id, mut saved) in self.undiscovered_cooldowns.clone() {
            if saved.retain_unmatched(now) {
                match accounts.entry(id) {
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        entry.insert(saved);
                    }
                    std::collections::btree_map::Entry::Occupied(entry) => unmatched_accounts.push((entry.key().clone(), saved)),
                }
            }
        }
        CooldownState { version: 1, accounts, unmatched_accounts }
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
                    if a.observed_plan.is_none() {
                        a.observed_plan.clone_from(&old.observed_plan);
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
            self.apply_account_settings(&mut a);
            self.accounts.insert(a.id.clone(), a);
        }
        self.propagate_api_key_conflicts();
        self.restore_discovered_cooldowns(Utc::now());
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
        bounded_identifier(filter.model).then(|| self.evaluate_eligibility(filter, now))
    }

    fn evaluate_eligibility<'a>(&'a self, filter: &AccountFilter<'a>, now: DateTime<Utc>) -> ModelEligibility<'a> {
        let lane = filter.lane;
        let model = filter.model;
        let required_project = filter.required_project;
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
                let account_reset;
                (reasons, account_reset) = account.eligibility_blockers(now, QuotaTarget::Model(model));
                if reasons.is_empty() {
                    evaluation.eligible_accounts += 1;
                } else if let Some(reset) = account_reset {
                    evaluation.verified_next_reset_at = Some(evaluation.verified_next_reset_at.map_or(reset, |old| old.min(reset)));
                }
            }
            evaluation.accounts.push(AccountEligibility { auth_index: &account.auth_index, exclusion_reasons: reasons });
        }
        if evaluation.eligible_accounts > 0 {
            evaluation.verified_next_reset_at = None;
        }
        evaluation
    }

    pub(crate) fn model_eligibility_summary(&self, lane: Lane, model: Option<&str>, now: DateTime<Utc>) -> Value {
        let mut supported = 0;
        let mut eligible = 0;
        let mut total: f64 = 0.0;
        let mut available: f64 = 0.0;
        let mut best_effort = None;
        let mut verified = None;
        let mut reasons = BTreeSet::new();
        for account in self.accounts.values().filter(|a| {
            a.lane == lane && !a.disabled && model.is_none_or(|model| a.models.is_empty() || a.models.iter().any(|id| id == model))
        }) {
            supported += 1;
            let capacity = account.capacity_multiplier();
            total = (total + capacity).min(f64::MAX);
            if account.weight == 0 {
                reasons.insert("zero_weight");
                continue;
            }
            if account.api_key && self.policy.api_key_order == ApiKeyOrder::SubscriptionsOnly {
                reasons.insert("subscriptions_only");
                continue;
            }
            let scopes: BTreeSet<_>;
            let targets = if let Some(model) = model {
                vec![QuotaTarget::Model(model)]
            } else if !account.models.is_empty() {
                account.models.iter().map(|m| QuotaTarget::Model(m)).collect()
            } else {
                scopes = account
                    .windows
                    .iter()
                    .filter(|w| w.kind != QuotaWindowKind::Spend && w.unit != QuotaUnit::Credits)
                    .filter_map(|w| w.scope_model.as_deref())
                    .chain(account.scoped_cooldowns.iter().filter(|(_, c)| c.until > now).map(|(scope, _)| scope.as_str()))
                    .chain(
                        account
                            .pending_quota
                            .iter()
                            .filter(|p| p.provisional.is_some_and(|c| c.until > now))
                            .filter_map(|p| p.scope.as_deref()),
                    )
                    .map(|scope| if lane == Lane::Google { google_bucket(scope) } else { scope.to_owned() })
                    .collect();
                if scopes.is_empty() {
                    vec![QuotaTarget::Model("")]
                } else {
                    scopes.iter().map(|scope| QuotaTarget::Bucket(lane, scope)).collect()
                }
            };
            let mut best_capacity = None;
            for target in targets {
                let (blockers, proof) = account.eligibility_blockers(now, target);
                if blockers.is_empty() {
                    let mut active = account.percent_windows(target).filter(|w| quota_window_active(lane, w, now)).peekable();
                    // No quota telemetry is nominal capacity; an observed but unmeasured bucket is not.
                    let nominal = if active.peek().is_some() { 0.0 } else { 100.0 };
                    let remaining = active.filter_map(|w| account.effective_percent(w, now)).reduce(f64::min).unwrap_or(nominal);
                    // Any supported model can use this account; do not sum shared capacity across buckets.
                    best_capacity = Some(best_capacity.map_or(remaining, |old: f64| old.max(remaining)));
                } else {
                    reasons.extend(blockers.into_iter().map(|reason| match reason {
                        "global_cooldown" | "scoped_cooldown" => "cooldown",
                        "quota_window" => "quota_exhausted",
                        "auth_cooldown" => "auth_expired",
                        "quarantined" if account.quota_backoff(now).is_some() => "quota_exhausted",
                        other => other,
                    }));
                    verified = verified.into_iter().chain(proof).min();
                    best_effort = best_effort.into_iter().chain(account.bounded_recovery(now, target)).min();
                }
            }
            if let Some(remaining) = best_capacity {
                eligible += 1;
                available = (available + capacity * (remaining / 100.0)).min(f64::MAX);
            }
        }
        if supported == 0 {
            reasons.insert("no_supported_accounts");
        }
        if eligible != 0 {
            reasons.clear();
            best_effort = None;
            verified = None;
        }
        json!({
            "lane": match lane { Lane::Google => "google", Lane::Go => "go", _ => lane.name() },
            "model": model,
            "eligible_accounts": eligible,
            "supported_accounts": supported,
            "available_capacity_units": available,
            "total_capacity_units": total,
            "verified_next_reset_at": verified,
            "best_effort_next_reset_at": best_effort,
            "blocking_reasons": reasons,
        })
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

    #[cfg(test)]
    pub fn next_reset(&self, filter: &AccountFilter<'_>, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.supported(filter).filter(|a| !a.quarantined(now)).filter_map(|a| a.quota_limited_until(now, filter.model)).min()
    }

    #[cfg(test)]
    pub fn all_quota_limited(&self, filter: &AccountFilter<'_>, now: DateTime<Utc>) -> bool {
        let mut accounts = self.supported(filter).peekable();
        accounts.peek().is_some() && accounts.all(|a| !a.quarantined(now) && a.quota_limited_until(now, filter.model).is_some())
    }

    pub(crate) fn transient_recovery(&self, filter: &AccountFilter<'_>, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.supported(filter)
            .filter(|a| a.quota_limited_until(now, filter.model).is_none())
            .filter_map(|a| a.transient_recovery(now))
            .filter(|at| *at - now <= chrono::TimeDelta::seconds(MAX_COOLDOWN_SECS))
            .min()
    }

    pub(crate) fn quota_recovery(&self, filter: &AccountFilter<'_>, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.supported(filter)
            .filter(|a| a.active_quarantine(now).is_none_or(|e| e.kind.is_transient() || e.kind == AccountErrorKind::QuotaExhausted))
            .filter_map(|a| Some(a.quota_limited_until(now, filter.model)?.max(a.transient_recovery(now).unwrap_or(now))))
            .min()
    }

    pub(crate) fn bounded_quota_recovery(&self, filter: &AccountFilter<'_>, now: DateTime<Utc>, witness: bool) -> Option<DateTime<Utc>> {
        let target = QuotaTarget::Model(filter.model);
        self.supported(filter)
            .filter(|a| a.quota_limited_until_for(now, target).is_some())
            .filter(|a| {
                !witness || {
                    a.quota_backoff(now).is_none()
                        && a.active_quota_windows(now, target)
                            .all(|w| w.resets_at.is_some() && !w.stale && w.observation_state == QuotaObservationState::Measured)
                }
            })
            .filter_map(|a| a.bounded_recovery(now, target))
            .min()
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
        affinity: (Option<&str>, Option<&str>),
        tried: &[String],
        now: DateTime<Utc>,
    ) -> Option<Account> {
        self.pick_with_quota(filter, QuotaTarget::Model(filter.model), affinity, tried, now)
    }

    fn pick_with_quota(
        &mut self,
        filter: &AccountFilter<'_>,
        target: QuotaTarget<'_>,
        (session, parent): (Option<&str>, Option<&str>),
        tried: &[String],
        now: DateTime<Utc>,
    ) -> Option<Account> {
        let lane = filter.lane;
        let eligible = |a: &&Account| filter.supports_for(self, a, target) && a.eligible_for_quota(now, target) && !tried.contains(&a.id);
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
        let soonest = |a: &Account| a.soonest_reset(now, target);
        // Antigravity subscriptions drain the soonest weekly reset first: a session bound to a
        // later-resetting account moves back once that account's 5-hour window frees up.
        let drain_soonest = lane == Lane::Google && self.policy.strategy == RouteStrategy::SoonestReset;
        let sticky = session.and_then(bound).or_else(|| parent.and_then(bound));
        if self.policy.strategy == RouteStrategy::CapacityBalanced {
            let counts = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
            let scored: Vec<_> =
                options.into_iter().map(|a| (a, a.capacity_score(now, target, counts.get(&a.id).copied().unwrap_or_default()))).collect();
            if let Some((a, _)) = [session, parent]
                .into_iter()
                .flatten()
                .filter_map(bound)
                .find_map(|id| scored.iter().find(|(a, s)| a.id == id && (s.session_left.is_none() || s.ample())))
            {
                return Some((*a).clone());
            }
            let active_ample = scored.iter().filter(|(_, s)| s.active && s.ample());
            let best_active_rate = active_ample.clone().filter_map(|(_, s)| s.rate).reduce(f64::max);
            let best_active_tier = active_ample.map(|(_, s)| s.multiplier).reduce(f64::max);
            return scored
                .into_iter()
                .filter(|(_, s)| {
                    !(s.idle
                        && s.ample()
                        && s.rate.zip(best_active_rate).is_some_and(|(idle, active)| idle <= active)
                        && best_active_tier.is_some_and(|tier| s.multiplier <= tier))
                })
                .max_by(|(a, left), (b, right)| {
                    left.ample()
                        .cmp(&right.ample())
                        .then_with(|| match (left.rate, right.rate) {
                            (Some(a), Some(b)) => a.total_cmp(&b),
                            (None, None) => {
                                let nominal = |s: &CapacityScore| s.multiplier / (1.0 + s.in_flight as f64);
                                nominal(left).total_cmp(&nominal(right))
                            }
                            _ => left.rate.is_some().cmp(&right.rate.is_some()),
                        })
                        .then_with(|| left.session_capacity.total_cmp(&right.session_capacity))
                        .then_with(|| a.priority.cmp(&b.priority))
                        .then_with(|| right.in_flight.cmp(&left.in_flight))
                        .then_with(|| b.id.cmp(&a.id))
                })
                .map(|(a, _)| a.clone());
        }
        if let Some(a) = sticky.and_then(|id| options.iter().find(|a| a.id == id))
            && (!drain_soonest || options.iter().all(|b| soonest(b).0 >= soonest(a).0))
        {
            return Some((*a).clone());
        }
        let first = *options.first()?;
        match self.policy.strategy {
            RouteStrategy::CapacityBalanced => unreachable!(),
            RouteStrategy::SoonestReset => options.sort_by_key(|a| soonest(a)),
            RouteStrategy::FillFirst => options.sort_by(|a, b| {
                b.priority
                    .cmp(&a.priority)
                    .then_with(|| b.max_used_percent(now, target).total_cmp(&a.max_used_percent(now, target)))
                    .then_with(|| b.success.cmp(&a.success))
            }),
            RouteStrategy::RoundRobin | RouteStrategy::WeightedRoundRobin => {
                let weighted = self.policy.strategy == RouteStrategy::WeightedRoundRobin;
                let weight = |a: &&Account| match (weighted, a.weight) {
                    (false, _) => 1,
                    (true, 1) => u64::from(a.capacity_multiplier().round().clamp(1.0, f64::from(u32::MAX)) as u32),
                    (true, weight) => u64::from(weight),
                };
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
            in_flight: self.in_flight.clone(),
            ..Self::default()
        };
        let now = Utc::now();
        if lane == Lane::Google {
            copy.pick_with_quota(&AccountFilter::shared(lane, "", None), QuotaTarget::GoogleGemini, (None, None), &[], now)
        } else {
            copy.pick(lane, (None, None), "", None, &[], now)
        }
        .map(|a| a.id)
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
                if a.lane == Lane::Codex
                    && !a.api_key
                    && a.chatgpt_id.is_some()
                    && pool.accounts.values().any(|old| {
                        old.id != a.id
                            && !old.local
                            && !old.api_key
                            && old.lane == Lane::Codex
                            && old.chatgpt_id == a.chatgpt_id
                            && old.chatgpt_user == a.chatgpt_user
                    })
                {
                    return None;
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
        apply_account_settings(&pool.account_defaults, &pool.account_overrides, existing);
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
        pool.restore_discovered_cooldowns(Utc::now());
        return;
    }
    let source = format!("provenance:{label}");
    add_with_api_key(pool, Lane::Google, &source, raw, candidate_api_key);
    let id = local_credential_name(Lane::Google, &source);
    if let Some(account) = pool.accounts.get_mut(&id) {
        account.provenance_enrollment = Some(label.to_owned());
        if !account.access.is_empty() {
            account.provenance_token_fp = Some(Sha256::digest(&account.access).into());
        }
        pool.refresh_account_settings(&id);
        pool.restore_discovered_cooldowns(Utc::now());
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
    let list =
        rows.as_array().into_iter().flatten().filter_map(|row| Some((text(row, &["integrationID", "provider", "providerID", "id"])?, row)));
    for (name, entry) in object.chain(list) {
        if let Some(lane) = Lane::parse(name).filter(|lane| !excluded.contains(lane)) {
            if let Some(value) = entry.get("value").filter(|v| v.is_object()) {
                // OpenCode V2 `auth export` entry: {id, integrationID, label, active, value: {type, access, refresh, expires, methodID, metadata}}
                let (outer_api, outer_oauth) = record_api_key_evidence(entry, lane);
                let (inner_api, inner_oauth) = record_api_key_evidence(value, lane);
                let force_api_key = outer_api || inner_api;
                let mut explicit_fps = extract_api_key_token_fps(entry, lane, force_api_key);
                explicit_fps.extend(extract_api_key_token_fps(value, lane, force_api_key));
                pool.merge_api_key_conflict_fps(&explicit_fps);
                if force_api_key && (outer_oauth || inner_oauth) {
                    continue;
                }
                let mut normalized = value.as_object().cloned().unwrap_or_default();
                if let Some(label) = text(entry, &["label"]) {
                    normalized.entry("label").or_insert_with(|| json!(label));
                }
                if let Some(account_id) = text(value, &["/metadata/accountID", "/metadata/account_id"]) {
                    normalized.entry("account_id").or_insert_with(|| json!(account_id));
                }
                if let Some(client_id) = text(value, &["/metadata/clientID", "/metadata/client_id"]) {
                    normalized.entry("client_id").or_insert_with(|| json!(client_id));
                }
                let key = text(entry, &["id", "label"]).map(|id| format!("{name}:{id}")).unwrap_or_else(|| name.to_owned());
                add_with_api_key(pool, lane, &key, Value::Object(normalized), force_api_key);
            } else if let Some(auth) = entry.get("auth") {
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
