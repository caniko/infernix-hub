//! Renewals: credential and account deadlines. The seed owns the facts
//! (title, procedure, guide URLs, cadence, warn window, expiry kind) and the
//! operator owns the clock: a re-seed never moves `due_at` or `renewed_at`.
//!
//! A `provider` renewal's health comes from the collector's observations. The
//! hub owns that judgement (D16/D17) so Today, the console, the tray, MCP and
//! the daily summary all read the same status.

use chrono::{DateTime, Duration, Utc};
use contracts::{ExpiryKind, Observation, ObserveRenewalIn, Renewal, RenewalRenewedIn, Role, TodayItem};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{Actor, DResult, DomainError, Hub, OPERATOR_OR_PROCEDURE, audit, decode, decode_one, invalid};

/// A provider answer is trusted for this long. Older evidence is unverifiable,
/// not healthy (D17).
const OBSERVATION_FRESH: Duration = Duration::seconds(48 * 60 * 60);
/// Clock difference tolerated between the collector and the hub.
const CLOCK_SKEW: Duration = Duration::seconds(60);

const SELECT: &str = "select to_jsonb(t) from (
    select r.id, r.company_id, r.key, r.account_id, r.title, r.procedure, r.urls,
      r.cadence_days, r.warn_days, r.due_at, r.renewed_at,
      r.expiry_kind, r.observation, r.observed_at, r.observed_expires_at,
      r.last_good_at, r.last_good_expires_at, r.created_at, r.updated_at
    from renewals r";

/// What the operator has to know about one renewal. `never` and `ok` are
/// statements, not to-dos.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenewalStatus {
    Ok,
    Due,
    Overdue,
    Unknown,
    Never,
    Unverifiable,
    Invalid,
}

impl RenewalStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Due => "due",
            Self::Overdue => "overdue",
            Self::Unknown => "unknown",
            Self::Never => "never",
            Self::Unverifiable => "unverifiable",
            Self::Invalid => "invalid",
        }
    }
}

/// The stored facts `status` judges. No clock read, no I/O.
#[derive(Debug, Clone)]
pub struct RenewalFacts {
    pub expiry_kind: ExpiryKind,
    pub warn_days: i32,
    pub due_at: Option<DateTime<Utc>>,
    pub observation: Option<Observation>,
    pub observed_at: Option<DateTime<Utc>>,
    /// The deadline the last successful observation reported, if any.
    pub last_good_expires_at: Option<DateTime<Utc>>,
}

/// D17: invalid > overdue > due > unverifiable > never/ok.
///
/// A provider's statement about the credential outranks every deadline, and a
/// deadline the last good observation reported stays visible when the current
/// attempt failed or went stale. Missing, stale or future-timestamped evidence
/// is unverifiable, never `never`.
pub fn status(facts: &RenewalFacts, now: DateTime<Utc>) -> RenewalStatus {
    match facts.expiry_kind {
        // An authored `never` is a type, not a date: no observation can
        // rewrite it.
        ExpiryKind::Never => RenewalStatus::Never,
        ExpiryKind::Cadence => match facts.due_at {
            None => RenewalStatus::Unknown,
            Some(due_at) => deadline(due_at, facts.warn_days, now),
        },
        ExpiryKind::Provider => provider_status(facts, now),
    }
}

fn provider_status(facts: &RenewalFacts, now: DateTime<Utc>) -> RenewalStatus {
    // A provider-confirmed inactive or not-yet-valid credential is invalid
    // even when the attempt that reported it is old: revocation does not heal.
    if matches!(facts.observation, Some(Observation::Inactive | Observation::NotYetValid)) {
        return RenewalStatus::Invalid;
    }
    let fresh = facts.observed_at.is_some_and(|at| at <= now + CLOCK_SKEW && now.signed_duration_since(at) <= OBSERVATION_FRESH);
    if !fresh {
        return last_good_deadline(facts, now).unwrap_or(RenewalStatus::Unverifiable);
    }
    match facts.observation {
        // `observe` writes a `date` answer to `last_good_expires_at` in the
        // same transaction; a missing one is malformed, so fail closed.
        Some(Observation::Date) => {
            facts.last_good_expires_at.map(|at| deadline(at, facts.warn_days, now)).unwrap_or(RenewalStatus::Unverifiable)
        }
        Some(Observation::Never) => RenewalStatus::Never,
        Some(Observation::Unverifiable | Observation::Inactive | Observation::NotYetValid) | None => {
            last_good_deadline(facts, now).unwrap_or(RenewalStatus::Unverifiable)
        }
    }
}

/// The last good deadline, only when it is itself something to act on.
fn last_good_deadline(facts: &RenewalFacts, now: DateTime<Utc>) -> Option<RenewalStatus> {
    facts
        .last_good_expires_at
        .map(|at| deadline(at, facts.warn_days, now))
        .filter(|status| matches!(status, RenewalStatus::Due | RenewalStatus::Overdue))
}

/// The deadline itself is due/overdue; exactly `warn_days` ahead still warns.
fn deadline(expires_at: DateTime<Utc>, warn_days: i32, now: DateTime<Utc>) -> RenewalStatus {
    if expires_at <= now {
        RenewalStatus::Overdue
    } else if expires_at <= now + Duration::days(i64::from(warn_days.max(0))) {
        RenewalStatus::Due
    } else {
        RenewalStatus::Ok
    }
}

fn facts(row: &Renewal) -> RenewalFacts {
    RenewalFacts {
        expiry_kind: row.expiry_kind,
        warn_days: row.warn_days,
        due_at: row.due_at,
        observation: row.observation,
        observed_at: row.observed_at,
        last_good_expires_at: row.last_good_expires_at,
    }
}

/// Fill the derived `status`; every read path returns the same judgement.
fn judged(mut row: Renewal, now: DateTime<Utc>) -> Renewal {
    row.status = status(&facts(&row), now).as_str().to_string();
    row
}

/// The deadline behind a row's status: the operator's date for cadence kinds,
/// the last good provider date for provider kinds.
fn deadline_of(row: &Renewal) -> Option<DateTime<Utc>> {
    match row.expiry_kind {
        ExpiryKind::Provider => row.last_good_expires_at,
        _ => row.due_at,
    }
}

pub(crate) async fn all(hub: &Hub, owner: Option<&str>) -> DResult<Vec<Renewal>> {
    let rows: Vec<Value> =
        // ponytail: unbounded read; renewals are a hand-written catalogue (tens of rows). Paginate if that changes.
        sqlx::query_scalar(&format!("{SELECT} where r.company_id = $1 and r.owner is not distinct from $2 order by r.due_at nulls first, r.title) t"))
            .bind(&hub.config.company_id)
            .bind(owner)
            .fetch_all(&hub.db)
            .await?;
    let now = Utc::now();
    Ok(decode(rows)?.into_iter().map(|row| judged(row, now)).collect())
}

pub async fn list(hub: &Hub, actor: &Actor) -> DResult<Vec<Renewal>> {
    actor.require(&[Role::Operator, Role::Agent, Role::Member])?;
    all(hub, (actor.principal.role == Role::Member).then_some(actor.principal.name.as_str())).await
}

/// The Today bucket: every renewal the operator must act on. `never` is a
/// console fact, not a to-do, so it never enters Today and never notifies
/// (D15).
pub(crate) async fn today(hub: &Hub) -> DResult<Vec<TodayItem>> {
    let mut items: Vec<TodayItem> = all(hub, None)
        .await?
        .into_iter()
        .filter(|row| !matches!(row.status.as_str(), "ok" | "never"))
        .map(|row| TodayItem {
            detail: None,
            kind: "renewal".into(),
            title: row.title.clone(),
            subject_type: "renewal".into(),
            subject_id: row.id,
            status: row.status.clone(),
            at: deadline_of(&row),
        })
        .collect();
    // Most urgent first, so the cut never drops an overdue row behind unknowns.
    let rank = |status: &str| match status {
        "invalid" | "overdue" => 0,
        "due" | "unverifiable" => 1,
        _ => 2,
    };
    items.sort_by(|a, b| rank(&a.status).cmp(&rank(&b.status)).then_with(|| a.at.cmp(&b.at)).then_with(|| a.title.cmp(&b.title)));
    items.truncate(50);
    Ok(items)
}

/// A human may renew their own row. An explicit due date wins; otherwise the
/// stored cadence starts the next clock.
pub async fn renewed(hub: &Hub, actor: &Actor, key: &str, input: RenewalRenewedIn) -> DResult<Renewal> {
    actor.require(&[Role::Operator, Role::Member])?;
    let mut tx = hub.db.begin().await?;
    let cadence: Option<Option<i32>> = sqlx::query_scalar(
        "select cadence_days from renewals where company_id = $1 and key = $2 and owner is not distinct from $3 for update",
    )
    .bind(&hub.config.company_id)
    .bind(key)
    .bind((actor.principal.role == Role::Member).then_some(actor.principal.name.as_str()))
    .fetch_optional(&mut *tx)
    .await?;
    let cadence = cadence.ok_or(DomainError::NotFound("renewal"))?;
    if input.due_at.is_none() && cadence.is_none() {
        return Err(invalid("due_required", "renewal has no cadence_days; pass due_at"));
    }
    let id: Uuid = sqlx::query_scalar(
        "update renewals
         set renewed_at = now(), due_at = coalesce($3, now() + make_interval(days => cadence_days)), updated_at = now()
         where company_id = $1 and key = $2
         returning id",
    )
    .bind(&hub.config.company_id)
    .bind(key)
    .bind(input.due_at)
    .fetch_one(&mut *tx)
    .await?;
    audit(&mut tx, actor, "renewal.renewed", "renewal", id, json!({ "key": key, "due_at": input.due_at })).await?;
    let row: Value = sqlx::query_scalar(&format!("{SELECT} where r.id = $1) t")).bind(id).fetch_one(&mut *tx).await?;
    let row = judged(decode_one(row)?, Utc::now());
    tx.commit().await?;
    Ok(row)
}

/// Record one provider answer. Operator or procedure: a collector observes, it
/// never renews, so `due_at` and `renewed_at` stay untouched (D16).
pub async fn observe(hub: &Hub, actor: &Actor, key: &str, input: ObserveRenewalIn) -> DResult<Renewal> {
    actor.require(OPERATOR_OR_PROCEDURE)?;
    let mut tx = hub.db.begin().await?;
    let current: Option<(String, Option<DateTime<Utc>>)> = sqlx::query_as(
        "select expiry_kind, observed_at from renewals where company_id = $1 and key = $2 and ($3 or owner is null) for update",
    )
    .bind(&hub.config.company_id)
    .bind(key)
    .bind(actor.principal.role == Role::Procedure)
    .fetch_optional(&mut *tx)
    .await?;
    let (kind, last_observed) = current.ok_or(DomainError::NotFound("renewal"))?;
    if kind != ExpiryKind::Provider.as_str() {
        return Err(invalid("not_provider", "only a provider expiry kind accepts observations"));
    }
    match (input.outcome, input.expires_at) {
        (Observation::Date, None) => return Err(invalid("expires_at_required", "outcome date requires expires_at")),
        (Observation::Date, Some(_)) => {}
        (_, Some(_)) => return Err(invalid("expires_at_unexpected", "expires_at is only accepted with outcome date")),
        (_, None) => {}
    }
    if input.observed_at > Utc::now() + CLOCK_SKEW {
        return Err(invalid("observed_in_future", "observed_at is more than 60 s in the future"));
    }
    // A collector reports what it just saw. An answer already past the
    // freshness window is not evidence (status() would call it stale anyway),
    // and accepting it could replace retained good evidence and hide a crossed
    // deadline, so it is refused before it can touch the row.
    if Utc::now() - input.observed_at > OBSERVATION_FRESH {
        return Err(invalid("observation_stale", "observed_at is older than the 48 h freshness window"));
    }
    // Under the row lock: an older (or replayed) answer must not replace newer
    // evidence, or it could erase a crossed deadline. PostgreSQL keeps
    // microseconds, so compare (and store) at that precision or a replay of the
    // same instant would look newer.
    let observed_at = chrono::SubsecRound::trunc_subsecs(input.observed_at, 6);
    if last_observed.is_some_and(|last| observed_at <= last) {
        return Err(invalid("observation_out_of_order", "observed_at is not newer than the stored observation"));
    }
    let outcome = input.outcome.as_str();
    let id: Uuid = sqlx::query_scalar(
        "update renewals set
           observed_at = $3,
           observation = $4,
           observed_expires_at = case when $4 = 'date' then $5 end,
           last_good_at = case when $4 in ('date', 'never') then $3 else last_good_at end,
           last_good_expires_at = case when $4 = 'date' then $5 when $4 = 'never' then null else last_good_expires_at end,
           last_good_never = case when $4 = 'date' then false when $4 = 'never' then true else last_good_never end,
           updated_at = now()
         where company_id = $1 and key = $2
         returning id",
    )
    .bind(&hub.config.company_id)
    .bind(key)
    .bind(observed_at)
    .bind(outcome)
    .bind(input.expires_at)
    .fetch_one(&mut *tx)
    .await?;
    // No secret material exists here; the audit still records only the typed
    // answer, never a request body.
    audit(
        &mut tx,
        actor,
        "renewal.observed",
        "renewal",
        id,
        json!({ "key": key, "outcome": outcome, "expires_at": input.expires_at, "observed_at": observed_at }),
    )
    .await?;
    let row: Value = sqlx::query_scalar(&format!("{SELECT} where r.id = $1) t")).bind(id).fetch_one(&mut *tx).await?;
    let row = judged(decode_one(row)?, Utc::now());
    tx.commit().await?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: &str = "2026-11-15T00:00:00Z";

    fn now() -> DateTime<Utc> {
        NOW.parse().unwrap()
    }

    fn at(raw: &str) -> DateTime<Utc> {
        raw.parse().unwrap()
    }

    fn cadence(due_at: Option<&str>, warn_days: i32) -> RenewalFacts {
        RenewalFacts {
            expiry_kind: ExpiryKind::Cadence,
            warn_days,
            due_at: due_at.map(at),
            observation: None,
            observed_at: None,
            last_good_expires_at: None,
        }
    }

    fn provider(observation: Option<Observation>, observed_at: Option<&str>, last_good_expires_at: Option<&str>) -> RenewalFacts {
        RenewalFacts {
            expiry_kind: ExpiryKind::Provider,
            warn_days: 14,
            due_at: None,
            observation,
            observed_at: observed_at.map(at),
            last_good_expires_at: last_good_expires_at.map(at),
        }
    }

    #[test]
    fn renewal_status_precedence() {
        let judge = |facts: &RenewalFacts| status(facts, now());

        // Cadence is today's clock: unknown, then the warn window, then ok.
        assert_eq!(judge(&cadence(None, 14)), RenewalStatus::Unknown);
        assert_eq!(judge(&cadence(Some(NOW), 14)), RenewalStatus::Overdue);
        assert_eq!(judge(&cadence(Some("2026-11-15T00:00:01Z"), 14)), RenewalStatus::Due);
        assert_eq!(judge(&cadence(Some("2026-11-29T00:00:00Z"), 14)), RenewalStatus::Due);
        assert_eq!(judge(&cadence(Some("2026-11-29T00:00:01Z"), 14)), RenewalStatus::Ok);

        // `never` is a kind, not a missing date: no observation can rewrite it.
        let mut never = cadence(None, 14);
        never.expiry_kind = ExpiryKind::Never;
        assert_eq!(judge(&never), RenewalStatus::Never);
        never.observation = Some(Observation::Inactive);
        never.observed_at = Some(now());
        assert_eq!(judge(&never), RenewalStatus::Never, "an authored never outranks a provider statement");

        // A provider with no evidence is unverifiable, never healthy and never
        // promoted to never.
        assert_eq!(judge(&provider(None, None, None)), RenewalStatus::Unverifiable);

        // Fresh active evidence judges the reported date normally.
        assert_eq!(
            judge(&provider(Some(Observation::Date), Some("2026-11-14T23:00:00Z"), Some("2026-11-01T00:00:00Z"))),
            RenewalStatus::Overdue
        );
        assert_eq!(judge(&provider(Some(Observation::Date), Some(NOW), Some("2026-11-20T00:00:00Z"))), RenewalStatus::Due);
        assert_eq!(judge(&provider(Some(Observation::Date), Some(NOW), Some("2027-03-01T00:00:00Z"))), RenewalStatus::Ok);

        // A fresh explicit null expiry is the explicit non-expiring answer.
        assert_eq!(judge(&provider(Some(Observation::Never), Some(NOW), None)), RenewalStatus::Never);

        // Stale success is unverifiable, unless its deadline already crossed
        // or is inside the warn window: a known deadline is never hidden.
        let stale = "2026-11-12T23:00:00Z";
        assert_eq!(judge(&provider(Some(Observation::Date), Some(stale), Some("2027-03-01T00:00:00Z"))), RenewalStatus::Unverifiable);
        assert_eq!(judge(&provider(Some(Observation::Date), Some(stale), Some("2026-11-01T00:00:00Z"))), RenewalStatus::Overdue);
        assert_eq!(judge(&provider(Some(Observation::Date), Some(stale), Some("2026-11-20T00:00:00Z"))), RenewalStatus::Due);
        assert_eq!(
            judge(&provider(Some(Observation::Never), Some(stale), None)),
            RenewalStatus::Unverifiable,
            "a stale never answer is not current evidence"
        );

        // A failed attempt is not success evidence, but the last good deadline
        // it already reported stays due/overdue.
        assert_eq!(judge(&provider(Some(Observation::Unverifiable), Some(NOW), Some("2027-03-01T00:00:00Z"))), RenewalStatus::Unverifiable);
        assert_eq!(judge(&provider(Some(Observation::Unverifiable), Some(NOW), Some("2026-11-01T00:00:00Z"))), RenewalStatus::Overdue);
        assert_eq!(judge(&provider(Some(Observation::Unverifiable), Some(NOW), Some("2026-11-20T00:00:00Z"))), RenewalStatus::Due);

        // A crossed deadline survives a missing observation too.
        assert_eq!(judge(&provider(None, None, Some("2026-11-01T00:00:00Z"))), RenewalStatus::Overdue);

        // An explicit provider judgement of the credential outranks the
        // deadline it also reports, even when the attempt is old.
        for stale_at in [NOW, stale] {
            assert_eq!(judge(&provider(Some(Observation::Inactive), Some(stale_at), Some("2027-03-01T00:00:00Z"))), RenewalStatus::Invalid);
            assert_eq!(
                judge(&provider(Some(Observation::NotYetValid), Some(stale_at), Some("2026-11-01T00:00:00Z"))),
                RenewalStatus::Invalid
            );
        }

        // Evidence from the future is evidence of a broken clock, not health.
        assert_eq!(
            judge(&provider(Some(Observation::Date), Some("2026-11-15T00:01:01Z"), Some("2027-03-01T00:00:00Z"))),
            RenewalStatus::Unverifiable
        );
        // Ordinary skew is tolerated, and exactly 48 h is still fresh.
        assert_eq!(
            judge(&provider(Some(Observation::Date), Some("2026-11-15T00:00:30Z"), Some("2027-03-01T00:00:00Z"))),
            RenewalStatus::Ok
        );
        assert_eq!(
            judge(&provider(Some(Observation::Date), Some("2026-11-13T00:00:00Z"), Some("2027-03-01T00:00:00Z"))),
            RenewalStatus::Ok
        );
        assert_eq!(
            judge(&provider(Some(Observation::Date), Some("2026-11-12T23:59:59Z"), Some("2027-03-01T00:00:00Z"))),
            RenewalStatus::Unverifiable
        );

        // A malformed date answer (no stored deadline) fails closed.
        assert_eq!(judge(&provider(Some(Observation::Date), Some(NOW), None)), RenewalStatus::Unverifiable);
    }
}
