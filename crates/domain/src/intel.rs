//! Policy-scoped intelligence. Every read uses current configuration; the
//! `intel_sources` row is the durable tenant binding, not a grant.

mod ingest;
mod links;

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use contracts::*;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{PgConnection, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{Actor, DResult, DomainError, Hub, audit, conflict, decode, decode_one, invalid};

pub use ingest::ingest_batch;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IntelConfig {
    pub sources: BTreeMap<String, IntelSourcePolicy>,
    pub grants: BTreeMap<String, IntelGrant>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntelSourcePolicy {
    pub company_id: String,
    pub default_audience: Audience,
    #[serde(default = "first_version")]
    pub policy_version: i32,
    pub kinds: Vec<SignalKind>,
    #[serde(default)]
    pub trust_tier: IntelTrustTier,
    pub stale_after_secs: u64,
    pub max_signals_per_day: u32,
}

fn first_version() -> i32 {
    1
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntelTrustTier {
    Observed,
    #[default]
    Derived,
    Llm,
    UntrustedWeb,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntelGrant {
    pub role: Role,
    pub principal_id: Option<Uuid>,
    pub company_id: String,
    pub max_audience: Audience,
    #[serde(default)]
    pub write_sources: Vec<String>,
    #[serde(default)]
    pub monitor_sources: Vec<String>,
}

fn valid_source(id: &str) -> bool {
    let lower_or_digit = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    id.len() <= 64 && id.bytes().next().is_some_and(lower_or_digit) && id.bytes().all(|c| lower_or_digit(c) || b"._-".contains(&c))
}

impl IntelConfig {
    pub fn validate(&self) -> DResult<()> {
        let bad = |message| invalid("invalid_intel_config", message);
        let valid_company = |id: &str| !id.trim().is_empty() && !id.chars().any(char::is_control);
        if self.sources.iter().any(|(id, p)| !valid_source(id) || !valid_company(&p.company_id) || p.policy_version < 1) {
            return Err(bad("invalid intel source id, company, or policy version"));
        }
        for grant in self.grants.values() {
            if !valid_company(&grant.company_id) || grant.max_audience == Audience::OperatorOnly {
                return Err(bad("invalid intel grant company or audience"));
            }
            if grant
                .write_sources
                .iter()
                .chain(&grant.monitor_sources)
                .any(|id| !self.sources.get(id).is_some_and(|p| p.company_id == grant.company_id))
            {
                return Err(bad("intel grants may only reference configured sources in their own company"));
            }
        }
        Ok(())
    }

    fn grant(&self, actor: &Actor) -> DResult<&IntelGrant> {
        let principal = &actor.principal;
        self.grants
            .get(&principal.name)
            .filter(|g| g.role == principal.role && g.principal_id.is_none_or(|id| id == principal.id))
            .ok_or(DomainError::Denied { code: "no_intel_grant", message: "caller has no matching intel grant".into() })
    }

    fn policy(&self, source: &str, company: &str) -> Option<&IntelSourcePolicy> {
        self.sources.get(source).filter(|policy| policy.company_id == company)
    }
}

enum Viewer<'a> {
    Operator(Option<String>),
    Granted(&'a IntelGrant),
}

impl<'a> Viewer<'a> {
    fn new(hub: &'a Hub, actor: &Actor) -> DResult<Self> {
        if actor.principal.role == Role::Operator {
            Ok(Self::Operator(hub.resolve_owner(actor).ok().filter(|owner| !owner.is_empty())))
        } else {
            Ok(Self::Granted(hub.config.intel.grant(actor)?))
        }
    }

    fn company(&self) -> Option<&str> {
        match self {
            Self::Operator(_) => None,
            Self::Granted(grant) => Some(&grant.company_id),
        }
    }
}

/// The reviewed content shared by the digest and both public read paths.
/// Server-issued identifiers and fixed public audience/state form the envelope.
const PUBLIC_PROJECTION_FIELDS: &[&str] =
    &["kind", "severity", "title", "summary", "facts", "subjects", "evidence", "confidence", "provenance", "occurred_at", "expires_at"];

/// Keep the audience test in SQL so paging/filtering happens after authority,
/// not after fetching a page of rows from other tenants. Unknown or rebound
/// policies are useful for operator cleanup only.
fn visible_sql(hub: &Hub, viewer: &Viewer<'_>) -> QueryBuilder<'static, Postgres> {
    let policies: Vec<_> = hub
        .config
        .intel
        .sources
        .iter()
        .map(
            |(id, p)| json!({"id": id, "company_id": p.company_id, "version": p.policy_version, "default_rank": p.default_audience.rank()}),
        )
        .collect();
    let mut sql = QueryBuilder::new(
        "select case when v.cross_company then
          (select jsonb_object_agg(key, value) from jsonb_each(to_jsonb(s)) where key = any(",
    );
    sql.push_bind(PUBLIC_PROJECTION_FIELDS).push(
        ")) || jsonb_build_object('id', s.id, 'company_id', s.company_id, 'source_id', s.source_id,
              'base_audience', 'public_cleared', 'effective_audience', 'public_cleared',
              'source_default_audience', 'public_cleared', 'cleared_audience', 'public_cleared', 'state', 'new')
          else (to_jsonb(s) - array['owner','owner_namespace','sticky_min_audience','cleared_revision',
              'cleared_base_audience','cleared_source_default_audience','cleared_policy_version','cleared_sha256',
              'dismissed_severity','operator_note','search'])
           || jsonb_build_object('base_audience', a.names[b.rank + 1], 'effective_audience', a.names[e.rank + 1],
              'source_default_audience', a.names[coalesce(p.default_rank, 3) + 1],
              'cleared_audience', case when c.valid then s.cleared_audience end, 'policy_version', coalesce(p.version, 0))",
    );
    if matches!(viewer, Viewer::Operator(_)) {
        sql.push(" || jsonb_build_object('operator_note', s.operator_note)");
    }
    sql.push(
        " end from intel_signals s
          cross join (select array['public_cleared','persona_eligible','company_internal','operator_only'] as names) a
          left join jsonb_to_recordset(",
    )
    .push_bind(json!(policies))
    .push(
        ") as p(id text, company_id text, version int, default_rank int)
             on p.id = s.source_id and p.company_id = s.company_id
           cross join lateral (select greatest(coalesce(p.default_rank, 3), array_position(a.names, s.sticky_min_audience) - 1) as rank) b
           cross join lateral (select (s.owner is null and s.cleared_audience is not null and s.cleared_revision = s.revision
              and s.cleared_policy_version = p.version and s.cleared_sha256 = s.projection_sha256
               and s.cleared_base_audience = a.names[b.rank + 1]
               and s.cleared_source_default_audience = a.names[p.default_rank + 1]) as valid) c
           cross join lateral (select case when c.valid then array_position(a.names, s.cleared_audience) - 1 else b.rank end as rank) e
           cross join lateral (select coalesce(s.company_id <> ",
    )
    .push_bind(viewer.company().map(str::to_owned))
    .push(", false) as cross_company) v where (s.owner is null");
    match viewer {
        Viewer::Operator(owner) => {
            sql.push(" or s.owner = ").push_bind(owner.clone());
        }
        Viewer::Granted(grant) => {
            sql.push(" and p.id is not null and (case when v.cross_company then c.valid and e.rank = 0 else e.rank <= ")
                .push_bind(i32::from(grant.max_audience.rank()))
                .push(" end)");
        }
    }
    sql.push(")");
    sql
}

async fn visible_signal(conn: &mut PgConnection, hub: &Hub, viewer: &Viewer<'_>, id: Uuid) -> DResult<Option<IntelSignal>> {
    let mut sql = visible_sql(hub, viewer);
    let row = sql.push(" and s.id = ").push_bind(id).build_query_scalar::<Value>().fetch_optional(conn).await?;
    row.map(decode_one).transpose()
}

fn severity_rank(severity: Severity) -> i32 {
    match severity {
        Severity::Info => 0,
        Severity::Warning => 1,
        Severity::Critical => 2,
    }
}

pub async fn list_signals(hub: &Hub, actor: &Actor, query: IntelSignalQuery) -> DResult<IntelSignalPage> {
    let viewer = Viewer::new(hub, actor)?;
    list_visible(hub, &viewer, query, false).await
}

async fn list_visible(hub: &Hub, viewer: &Viewer<'_>, query: IntelSignalQuery, today: bool) -> DResult<IntelSignalPage> {
    let mut sql = visible_sql(hub, viewer);
    if let Some(source) = query.source {
        if !valid_source(&source) {
            return Err(invalid("invalid_source", "invalid intel source id"));
        }
        sql.push(" and s.source_id = ").push_bind(source);
    }
    if let Some(kind) = query.kind {
        sql.push(" and s.kind = ").push_bind(kind.as_str());
    }
    if let Some(state) = query.state {
        sql.push(" and (case when v.cross_company then 'new' else s.state end) = ").push_bind(state.as_str());
    }
    if let Some(severity) = query.severity_min {
        sql.push(" and array_position(array['info','warning','critical'], s.severity) - 1 >= ").push_bind(severity_rank(severity));
    }
    if let Some(audience) = query.audience_at_most {
        sql.push(" and e.rank <= ").push_bind(i32::from(audience.rank()));
    }
    if let Some(persona) = query.persona {
        sql.push(" and not v.cross_company and s.persona_id = ").push_bind(persona);
    }
    if let Some(project) = query.project {
        sql.push(" and not v.cross_company and s.project_id = ").push_bind(project);
    }
    if let Some(subject) = query.subject {
        let subject = bounded(&subject, 2048, false, "invalid_subject")?;
        let needle =
            subject.split_once(':').and_then(|(scheme, value)| Some(json!({"scheme": SubjectScheme::parse(scheme)?, "value": value})));
        sql.push(" and s.subjects @> ").push_bind(json!([needle.unwrap_or_else(|| json!({"value": subject}))]));
    }
    if let Some(since) = query.since {
        sql.push(" and s.occurred_at >= ").push_bind(since);
    }
    if let Some(q) = query.q {
        let q = bounded(&q, 512, true, "invalid_search")?;
        if !q.is_empty() {
            sql.push(" and s.search @@ websearch_to_tsquery('simple', ").push_bind(q).push(")");
        }
    }
    if !query.include_expired {
        sql.push(" and (s.expires_at is null or s.expires_at > now())");
    }
    if today {
        sql.push(" and (v.cross_company or s.snoozed_until is null or s.snoozed_until <= now())");
    }
    if let Some(cursor) = query.cursor {
        let parsed = (|| {
            let (at, id) = cursor.split_once('/')?;
            Some((DateTime::parse_from_rfc3339(at).ok()?.with_timezone(&Utc), Uuid::parse_str(id).ok()?))
        })();
        let (at, id) = parsed.ok_or_else(|| invalid("invalid_cursor", "invalid intel pagination cursor"))?;
        sql.push(" and (s.occurred_at,s.id) < (").push_bind(at).push(",").push_bind(id).push(")");
    }
    let limit = query.limit.unwrap_or(50).clamp(1, 100);
    let rows = sql
        .push(" order by s.occurred_at desc, s.id desc limit ")
        .push_bind(i64::from(limit) + 1)
        .build_query_scalar::<Value>()
        .fetch_all(&hub.db)
        .await?;
    let mut items: Vec<IntelSignal> = decode(rows)?;
    let next_cursor = if items.len() > limit as usize {
        items.pop();
        items.last().map(|last| format!("{}/{}", last.occurred_at.to_rfc3339(), last.id))
    } else {
        None
    };
    Ok(IntelSignalPage { items, next_cursor })
}

pub(crate) async fn today_for_actor(hub: &Hub, actor: &Actor) -> DResult<Vec<TodayItem>> {
    let viewer = match Viewer::new(hub, actor) {
        Err(DomainError::Denied { code: "no_intel_grant", .. }) => return Ok(Vec::new()),
        other => other?,
    };
    let query = IntelSignalQuery { state: Some(SignalState::New), severity_min: Some(Severity::Warning), ..Default::default() };
    Ok(list_visible(hub, &viewer, query, true)
        .await?
        .items
        .into_iter()
        .map(|s| TodayItem {
            detail: None,
            kind: s.kind.to_string(),
            title: s.title,
            subject_type: "signal".into(),
            subject_id: s.id,
            status: s.severity.to_string(),
            at: Some(s.occurred_at),
        })
        .collect())
}

pub async fn get_signal(hub: &Hub, actor: &Actor, id: Uuid) -> DResult<IntelSignalDetail> {
    let viewer = Viewer::new(hub, actor)?;
    let mut tx = hub.db.begin().await?;
    sqlx::query("set transaction isolation level repeatable read, read only").execute(&mut *tx).await?;
    let signal = visible_signal(&mut tx, hub, &viewer, id).await?.ok_or(DomainError::NotFound("signal"))?;
    let links = if viewer.company().is_some_and(|company| company != signal.company_id) {
        Vec::new()
    } else {
        links::read(&mut tx, hub, actor, &viewer, &signal).await?
    };
    let revisions = if matches!(viewer, Viewer::Operator(_)) {
        decode(
            sqlx::query_scalar(
                "select to_jsonb(r) - 'signal_id' from intel_signal_revisions r where signal_id = $1 order by revision desc",
            )
            .bind(id)
            .fetch_all(&mut *tx)
            .await?,
        )?
    } else {
        Vec::new()
    };
    tx.commit().await?;
    Ok(IntelSignalDetail { signal, links, revisions })
}

async fn lock_for_review(conn: &mut PgConnection, hub: &Hub, viewer: &Viewer<'_>, id: Uuid) -> DResult<Option<String>> {
    let Viewer::Operator(owner) = viewer else {
        visible_signal(conn, hub, viewer, id).await?.ok_or(DomainError::NotFound("signal"))?;
        return Err(DomainError::Forbidden("only an operator can review signals".into()));
    };
    sqlx::query_scalar("select owner from intel_signals where id = $1 and (owner is null or owner = $2) for update")
        .bind(id)
        .bind(owner)
        .fetch_optional(conn)
        .await?
        .ok_or(DomainError::NotFound("signal"))
}

pub async fn decide_signal(hub: &Hub, actor: &Actor, id: Uuid, input: SignalDecisionIn) -> DResult<IntelSignal> {
    let viewer = Viewer::new(hub, actor)?;
    let mut tx = hub.db.begin().await?;
    lock_for_review(&mut tx, hub, &viewer, id).await?;
    let note = input.note.as_deref().map(|s| bounded(s, 4000, true, "invalid_note")).transpose()?;
    let update = match input.action {
        SignalDecisionAction::Acknowledge => "state = 'acknowledged', dismissed_severity = null, snoozed_until = null",
        SignalDecisionAction::Dismiss => "state = 'dismissed', dismissed_severity = severity, snoozed_until = null",
        SignalDecisionAction::Snooze => "snoozed_until = $3",
        SignalDecisionAction::Reopen => "state = 'new', dismissed_severity = null, snoozed_until = null",
    };
    let statement =
        format!("update intel_signals set updated_at = now(), operator_note = coalesce($2, operator_note), {update} where id = $1");
    let mut sql = sqlx::query(&statement).bind(id).bind(&note);
    if input.action == SignalDecisionAction::Snooze {
        let until = input.until.filter(|t| *t > Utc::now()).ok_or_else(|| invalid("invalid_snooze", "snooze requires a future until"))?;
        sql = sql.bind(until);
    }
    sql.execute(&mut *tx).await?;
    audit(&mut tx, actor, "intel.decision", "signal", id, json!({"action": input.action, "note": note, "until": input.until})).await?;
    let signal = visible_signal(&mut tx, hub, &viewer, id).await?.ok_or(DomainError::NotFound("signal"))?;
    tx.commit().await?;
    Ok(signal)
}

pub async fn clear_signal(hub: &Hub, actor: &Actor, id: Uuid, input: SignalClearanceIn) -> DResult<IntelSignal> {
    let viewer = Viewer::new(hub, actor)?;
    let mut tx = hub.db.begin().await?;
    if lock_for_review(&mut tx, hub, &viewer, id).await?.is_some() {
        return Err(invalid("owner_signal_cannot_widen", "owner-scoped signals cannot be cleared"));
    }
    let signal = visible_signal(&mut tx, hub, &viewer, id).await?.ok_or(DomainError::NotFound("signal"))?;
    let policy = hub.config.intel.policy(&signal.source_id, &signal.company_id);
    let review = signal.review.ok_or_else(|| DomainError::Internal("missing operator review metadata".into()))?;
    let Some(policy) = policy.filter(|p| {
        p.policy_version == input.policy_version
            && p.default_audience == input.source_default_audience
            && review.revision == input.revision
            && review.projection_sha256 == input.projection_sha256
    }) else {
        return Err(conflict("stale_revision", "signal revision, projection or policy has changed"));
    };
    let rationale = bounded(&input.rationale, 4000, false, "invalid_rationale")?;
    sqlx::query(
        "update intel_signals set cleared_audience = $2, cleared_revision = $3, cleared_policy_version = $4,
             cleared_sha256 = $5, cleared_base_audience = $6, cleared_source_default_audience = $7, updated_at = now() where id = $1",
    )
    .bind(id)
    .bind(input.audience.as_str())
    .bind(input.revision)
    .bind(input.policy_version)
    .bind(&input.projection_sha256)
    .bind(signal.base_audience.as_str())
    .bind(policy.default_audience.as_str())
    .execute(&mut *tx)
    .await?;
    audit(
        &mut tx,
        actor,
        "intel.clearance",
        "signal",
        id,
        json!({
            "audience": input.audience, "revision": input.revision, "policy_version": input.policy_version,
            "projection_sha256": input.projection_sha256, "base_audience": signal.base_audience, "rationale": rationale,
        }),
    )
    .await?;
    let signal = visible_signal(&mut tx, hub, &viewer, id).await?.ok_or(DomainError::NotFound("signal"))?;
    tx.commit().await?;
    Ok(signal)
}

pub async fn list_sources(hub: &Hub, actor: &Actor) -> DResult<Vec<IntelSourceStatus>> {
    let viewer = Viewer::new(hub, actor)?;
    let config = &hub.config.intel;
    let sources: Vec<_> = config
        .sources
        .iter()
        .filter_map(|(id, policy)| {
            let monitor = match &viewer {
                Viewer::Operator(_) => true,
                Viewer::Granted(grant) if grant.company_id == policy.company_id => grant.monitor_sources.contains(id),
                Viewer::Granted(_) => return None,
            };
            Some(json!({"id": id, "company_id": policy.company_id, "policy_version": policy.policy_version,
            "default_audience": policy.default_audience, "stale_after_secs": policy.stale_after_secs, "monitor": monitor}))
        })
        .collect();
    decode(
        sqlx::query_scalar(
            "select to_jsonb(t) from (
           select p.id, p.company_id, p.policy_version, p.default_audience, s.last_ingest_at, s.last_event_at, s.last_status,
             s.last_error_code, case when p.monitor then s.last_error_detail end as last_error_detail,
             case when p.monitor then coalesce(s.watermark, '{}'::jsonb) end as watermark,
             (s.last_ingest_at is null or extract(epoch from now() - s.last_ingest_at) >= p.stale_after_secs) as stale,
             p.stale_after_secs
           from jsonb_to_recordset($1) as p(id text, company_id text, policy_version int, default_audience text,
               stale_after_secs numeric, monitor bool)
           left join intel_sources s on s.id = p.id and s.company_id = p.company_id
           order by p.id
         ) t",
        )
        .bind(json!(sources))
        .fetch_all(&hub.db)
        .await?,
    )
}

fn bounded(input: &str, max: usize, empty: bool, code: &'static str) -> DResult<String> {
    if input.chars().count() > max {
        return Err(invalid(code, "intel text field exceeds its size limit"));
    }
    let cleaned: String = input.chars().filter(|c| !c.is_control()).collect();
    if !empty && cleaned.trim().is_empty() {
        return Err(invalid(code, "intel text field must not be empty"));
    }
    Ok(cleaned)
}
