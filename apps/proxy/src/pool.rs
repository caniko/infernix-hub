use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    env, fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use contracts::providers::*;
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
    pub expires: Option<DateTime<Utc>>,
    pub chatgpt_id: Option<String>,
    pub chatgpt_user: Option<String>,
    pub organization: Option<String>,
    pub google_client: Option<String>,
    pub google_secret: Option<String>,
    pub project: Option<String>,
    pub api_key: bool,
    pub base: Option<reqwest::Url>,
    pub local: bool,
    pub models: Vec<String>,
    pub cooldown: Option<DateTime<Utc>>,
    pub auth_cooldown: Option<DateTime<Utc>>,
    pub scoped_cooldowns: BTreeMap<String, DateTime<Utc>>,
    pub windows: Vec<QuotaWindow>,
    pub google_fallback_keys: BTreeSet<String>,
    pub grants: Vec<ResetGrant>,
    pub next_grant: Option<String>,
    pub observed: Option<DateTime<Utc>>,
    pub last_used: Option<DateTime<Utc>>,
    pub probe_failed: bool,
    pub probe_generation: u64,
    pub success: u64,
    pub failure: u64,
}

impl Account {
    pub(crate) fn needs_google_metadata(&self) -> bool {
        self.lane == Lane::Google && !self.disabled && !self.api_key && (self.email.is_none() || self.project.is_none())
    }

    pub(crate) fn same_credential(&self, other: &Self) -> bool {
        self.lane == other.lane
            && self.api_key == other.api_key
            && (self.lane != Lane::Codex || (self.chatgpt_id == other.chatgpt_id && self.chatgpt_user == other.chatgpt_user))
            && (self.refresh.is_none() || other.refresh.is_none() || self.refresh == other.refresh)
            && ((!self.access.is_empty() && self.access == other.access) || (self.refresh.is_some() && self.refresh == other.refresh))
    }

    pub fn eligible(&self, now: DateTime<Utc>, model: &str) -> bool {
        self.supports(model) && self.auth_cooldown.is_none_or(|at| at <= now) && self.quota_limited_until(now, model).is_none()
    }

    fn supports(&self, model: &str) -> bool {
        !self.disabled && self.weight > 0 && (model.is_empty() || self.models.is_empty() || self.models.iter().any(|id| id == model))
    }

    pub fn quota_limited_until(&self, now: DateTime<Utc>, model: &str) -> Option<DateTime<Utc>> {
        let scoped = model_cooldown_scope(self.lane, model).and_then(|scope| self.scoped_cooldowns.get(&scope).copied());
        self.windows
            .iter()
            .filter(|w| window_applies_to_model(w, model) && enforces_quota(w) && quota_window_active(self.lane, w, now))
            .map(|w| {
                w.resets_at.unwrap_or_else(|| {
                    if self.lane == Lane::Google {
                        w.observed_at.map_or(DateTime::<Utc>::MAX_UTC, |at| at + chrono::TimeDelta::hours(1))
                    } else {
                        DateTime::<Utc>::MAX_UTC
                    }
                })
            })
            .chain(self.cooldown.into_iter().chain(scoped).filter(|until| *until > now))
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
            "type": self.provider, "label": self.label, "disabled": self.disabled, "note": self.note, "priority": self.priority,
            "weight": self.weight, "status": if self.disabled { "disabled" } else if self.probe_failed { "error" } else { "active" },
            "cooldown_until": self.cooldown, "observed_at": self.observed, "last_used_at": self.last_used,
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
    ensure!(filename(name) && raw.is_object(), "invalid credential name or JSON object");
    let kind = text(raw, &["type", "provider"]).unwrap_or_default();
    let group = raw.get("api_key").filter(|v| v.is_object()).unwrap_or(raw);
    let lane = text(raw, &["family", "provider", "type"])
        .and_then(Lane::parse)
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
        let api_key = lane != Lane::Go
            && (key.is_some()
                || is_group
                || matches!(kind, "api_key" | "api-key")
                || (kind == "openai" && access.starts_with("sk-") && refresh.is_none()));
        let access_claims = jwt_claims(access);
        let claims = text(entry, &["id_token", "/tokens/id_token"]).map(jwt_claims).unwrap_or_else(|| access_claims.clone());
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
        Ok(Account {
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
            expires: ["expires_at", "expiresAt", "expiry", "expired", "expires", "/claudeAiOauth/expiresAt"]
                .into_iter()
                .find_map(|k| date(if k.starts_with('/') { entry.pointer(k).unwrap_or(&Value::Null) } else { &entry[k] }))
                .or_else(|| date(&jwt_claims(access)["exp"])),
            chatgpt_id,
            chatgpt_user,
            organization,
            google_client,
            google_secret,
            project: text(entry, &["project_id", "projectId", "project"]).map(str::to_owned),
            api_key,
            base: (api_key || lane == Lane::Go).then(|| base.clone()).flatten(),
            local,
            models: models.clone(),
            cooldown: None,
            auth_cooldown: None,
            scoped_cooldowns: BTreeMap::new(),
            windows: vec![],
            google_fallback_keys: BTreeSet::new(),
            grants: vec![],
            next_grant: None,
            observed: None,
            last_used: None,
            probe_failed: false,
            probe_generation: 0,
            success: 0,
            failure: 0,
        })
    };
    if let Some(keys) = group["keys"].as_array() {
        ensure!(!keys.is_empty() && keys.len() <= 256, "invalid key group size");
        keys.iter().enumerate().map(|(i, entry)| make(entry, format!("{name}#{}", i + 1), true)).collect()
    } else {
        Ok(vec![make(group, name.into(), false)?])
    }
}

type Session = (Lane, String);

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
}

impl Pool {
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

    pub fn reload_local(config: &Config, account: &Account) -> Option<Account> {
        if config.exclude_local_lanes.contains(&account.lane) {
            return None;
        }
        let mut found = Self::default();
        match account.lane {
            Lane::Claude => discover_claude(config, &mut found),
            Lane::Codex => discover_codex(config, &mut found),
            _ => return None,
        }
        discover_opencode(config, &mut found);
        found.accounts.remove(&account.id)
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
            if let Some(old) = self.accounts.get(&a.id) {
                if a.local && old.local && old.lane == a.lane {
                    a.disabled = old.disabled;
                    a.priority = old.priority;
                    a.weight = old.weight;
                    a.note.clone_from(&old.note);
                }
                if old.same_credential(&a) && old.access == a.access && old.refresh == a.refresh {
                    a.cooldown = old.cooldown;
                    a.auth_cooldown = old.auth_cooldown;
                    a.scoped_cooldowns.clone_from(&old.scoped_cooldowns);
                    a.windows.clone_from(&old.windows);
                    a.google_fallback_keys.clone_from(&old.google_fallback_keys);
                    a.grants.clone_from(&old.grants);
                    a.next_grant.clone_from(&old.next_grant);
                    a.observed = old.observed;
                    a.last_used = old.last_used;
                    a.probe_failed = old.probe_failed;
                    a.probe_generation = old.probe_generation;
                    a.success = old.success;
                    a.failure = old.failure;
                }
            }
            self.accounts.insert(a.id.clone(), a);
        }
    }

    pub fn find(&self, id: &str) -> Option<&Account> {
        self.accounts.get(id).or_else(|| self.accounts.values().find(|a| a.auth_index == id || self.hub_id(a) == id || a.hub_id() == id))
    }

    pub(crate) fn supports(&self, a: &Account, lane: Lane, model: &str, required_project: Option<&str>) -> bool {
        a.lane == lane
            && required_project.is_none_or(|project| a.project.as_deref() == Some(project))
            && a.supports(model)
            && !(a.api_key && self.policy.api_key_order == ApiKeyOrder::SubscriptionsOnly)
    }

    fn supported<'a>(&'a self, lane: Lane, model: &'a str, required_project: Option<&'a str>) -> impl Iterator<Item = &'a Account> {
        self.accounts.values().filter(move |a| self.supports(a, lane, model, required_project))
    }

    pub fn next_reset(&self, lane: Lane, model: &str, required_project: Option<&str>) -> Option<DateTime<Utc>> {
        let now = Utc::now();
        self.supported(lane, model, required_project).filter_map(|a| a.quota_limited_until(now, model)).min()
    }

    pub fn all_quota_limited(&self, lane: Lane, model: &str, required_project: Option<&str>) -> bool {
        let now = Utc::now();
        let mut accounts = self.supported(lane, model, required_project).peekable();
        accounts.peek().is_some() && accounts.all(|a| a.quota_limited_until(now, model).is_some())
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
        session: Option<&str>,
        parent: Option<&str>,
        model: &str,
        required_project: Option<&str>,
        tried: &[String],
    ) -> Option<Account> {
        let now = Utc::now();
        let eligible = |a: &&Account| self.supports(a, lane, model, required_project) && a.eligible(now, model) && !tried.contains(&a.id);
        if let Some(a) = self.preferred(lane).and_then(|id| self.accounts.get(id)).filter(eligible) {
            return Some(a.clone());
        }
        let mut options: Vec<_> = self.accounts.values().filter(eligible).collect();
        let keys_first = self.policy.api_key_order == ApiKeyOrder::KeysFirst;
        if options.iter().any(|a| a.api_key == keys_first) {
            options.retain(|a| a.api_key == keys_first);
        }
        let sticky = session.and_then(|id| self.bound(lane, id)).or_else(|| parent.and_then(|id| self.bound(lane, id)));
        if let Some(a) = sticky.and_then(|id| options.iter().find(|a| a.id == id)) {
            return Some((*a).clone());
        }
        let first = *options.first()?;
        match self.policy.strategy {
            RouteStrategy::SoonestReset => options.sort_by_key(|a| {
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
            }),
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
        copy.pick(lane, None, None, "", None, &[]).map(|a| a.id)
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

fn add(pool: &mut Pool, lane: Lane, label: &str, mut raw: Value) {
    let Some(fields) = raw.as_object_mut() else { return };
    fields.insert("type".into(), json!(lane.name()));
    fields.remove("provider");
    fields.remove("family");
    let name = format!("local-{}-{}.json", lane.name(), hex::encode(&Sha256::digest(label)[..8]));
    if let Ok(mut found) = parse_credentials(&name, &raw, true) {
        let cutoff = Utc::now() + chrono::TimeDelta::seconds(30);
        found.retain(|a| {
            if !a.api_key && matches!(a.lane, Lane::Claude | Lane::Codex) && a.expires.is_some_and(|e| e <= cutoff) {
                return false;
            }
            !pool.accounts.values().any(|old| {
                old.lane == a.lane
                    && ((!a.access.is_empty() && old.access == a.access)
                        || a.refresh.as_ref().is_some_and(|r| old.refresh.as_ref() == Some(r)))
            })
        });
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

pub(crate) fn add_provenance_google(pool: &mut Pool, label: &str, mut raw: Value) {
    let Some(fields) = raw.as_object_mut() else { return };
    fields.entry("label").or_insert_with(|| json!(label));
    let refresh = text(&raw, &["refresh_token", "refreshToken", "refresh"]).map(str::to_owned);
    let client = text(&raw, &["client_id", "clientId"]).map(str::to_owned);
    let secret = text(&raw, &["client_secret", "clientSecret"]).map(str::to_owned);
    let project = text(&raw, &["project_id", "projectId", "project"]).map(str::to_owned);
    let email = text(&raw, &["email"]).map(str::to_owned);
    if let Some(refresh) = refresh.as_deref()
        && let Some(existing) = pool.accounts.values_mut().find(|a| a.lane == Lane::Google && a.refresh.as_deref() == Some(refresh))
    {
        let relabel = existing.local
            && existing.label != label
            && (existing.label == existing.name || existing.label.starts_with("local-") || existing.label.len() == 64);
        let enriched = (existing.google_client.is_none() && client.is_some())
            || (existing.google_secret.is_none() && secret.is_some())
            || (existing.project.is_none() && project.is_some())
            || (existing.email.is_none() && email.is_some());
        if relabel {
            existing.label = label.to_owned();
        }
        if existing.google_client.is_none() {
            existing.google_client = client;
        }
        if existing.google_secret.is_none() {
            existing.google_secret = secret;
        }
        if existing.project.is_none() {
            existing.project = project;
        }
        if existing.email.is_none() {
            existing.email = email;
        }
        pool.revision += i64::from(relabel || enriched);
        return;
    }
    add(pool, Lane::Google, &format!("provenance:{label}"), raw);
}

pub(crate) fn add_opencode(pool: &mut Pool, raw: &Value, excluded: &BTreeSet<Lane>) {
    let rows = raw.get("credentials").unwrap_or(raw);
    let object = rows.as_object().into_iter().flatten().map(|(name, row)| (name.as_str(), row));
    let list = rows.as_array().into_iter().flatten().filter_map(|row| Some((text(row, &["provider", "providerID", "id"])?, row)));
    for (name, entry) in object.chain(list) {
        if let Some(lane) = Lane::parse(name).filter(|lane| !excluded.contains(lane)) {
            add(pool, lane, name, entry.get("auth").unwrap_or(entry).clone());
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
