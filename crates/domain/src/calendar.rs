use std::collections::BTreeSet;

use chrono::{DateTime, Datelike, Days, Months, NaiveDate, NaiveDateTime, TimeDelta, Utc};
use contracts::{CalendarAgendaResponse, CalendarEvent, CalendarEventIn, CalendarSource, CalendarSourceUpsert, CalendarSyncResult};
use sqlx::{PgConnection, types::Json};
use uuid::Uuid;

use crate::{Actor, DResult, DomainError, Hub, conflict, decode, decode_one, invalid};

impl Hub {
    pub async fn upsert_calendar_source_system(&self, actor: &Actor, owner: &str, input: &CalendarSourceUpsert) -> DResult<CalendarSource> {
        let owner = Self::sync_owner(actor, owner)?;
        if input.label.trim().is_empty()
            || !matches!(input.provider.as_str(), "google_ics" | "caldav" | "google_api" | "ics_url")
            || !matches!(input.visibility.as_str(), "private" | "shared")
            || !input.color.strip_prefix('#').is_some_and(|hex| hex.len() == 6 && hex.bytes().all(|c| c.is_ascii_hexdigit()))
        {
            return Err(invalid("calendar_source", "invalid calendar label, provider, visibility or color"));
        }
        decode_one(sqlx::query_scalar(
            "insert into calendar_sources as s (owner,label,provider,visibility,color,enabled,status)
             values ($1,$2,$3,$4,$5,$6,case when $6 then 'idle' else 'disabled' end)
             on conflict (owner,label) do update set provider=excluded.provider,visibility=excluded.visibility,color=excluded.color,
               enabled=excluded.enabled, status=case when not excluded.enabled then 'disabled' when not s.enabled then 'idle' else s.status end,
               sync_leased_until=case when excluded.enabled then s.sync_leased_until end, updated_at=now() returning to_jsonb(s)",
        ).bind(owner).bind(&input.label).bind(&input.provider).bind(&input.visibility).bind(&input.color).bind(input.enabled)
            .fetch_one(&self.db).await?)
    }

    pub async fn list_calendar_sources(&self, actor: &Actor) -> DResult<Vec<CalendarSource>> {
        let owner = self.resolve_owner(actor)?;
        decode(
            sqlx::query_scalar(
                "select case when owner=$1 then to_jsonb(s) else to_jsonb(s)-'last_error'-'sync_etag' end
             from calendar_sources s where owner=$1 or visibility='shared' order by label,id",
            )
            .bind(owner)
            .fetch_all(&self.db)
            .await?,
        )
    }

    pub async fn calendar_source_system(&self, actor: &Actor, owner: &str, label: &str) -> DResult<CalendarSource> {
        let owner = Self::sync_owner(actor, owner)?;
        let row = sqlx::query_scalar("select to_jsonb(s) from calendar_sources s where owner=$1 and label=$2")
            .bind(owner)
            .bind(label)
            .fetch_optional(&self.db)
            .await?
            .ok_or(DomainError::NotFound("calendar source"))?;
        decode_one(row)
    }

    /// Returns the exact database expiry as the fence for the acquired lease.
    pub async fn acquire_calendar_sync_lease(&self, actor: &Actor, owner: &str, id: Uuid, seconds: u32) -> DResult<Option<DateTime<Utc>>> {
        let owner = Self::sync_owner(actor, owner)?;
        let expires = sqlx::query_scalar(
            "update calendar_sources set sync_leased_until=clock_timestamp()+make_interval(secs=>$3)
             where owner=$1 and id=$2 and enabled and (sync_leased_until is null or sync_leased_until<=clock_timestamp())
             returning sync_leased_until",
        )
        .bind(owner)
        .bind(id)
        .bind(f64::from(seconds.clamp(1, 3600)))
        .fetch_optional(&self.db)
        .await?;
        if expires.is_none() {
            let mut conn = self.db.acquire().await?;
            calendar_lease(&mut conn, owner, id).await?;
        }
        Ok(expires)
    }

    /// Call only for a complete, successfully fetched snapshot. An empty
    /// complete snapshot clears events; a partial snapshot must never get here.
    pub async fn replace_calendar_source_events(
        &self,
        actor: &Actor,
        owner: &str,
        id: Uuid,
        lease: DateTime<Utc>,
        etag: Option<&str>,
        events: &[CalendarEventIn],
    ) -> DResult<CalendarSyncResult> {
        let owner = Self::sync_owner(actor, owner)?;
        let mut tx = self.db.begin().await?;
        let (visibility, active, expiry) = calendar_lease(&mut tx, owner, id).await?;
        if !active || expiry != Some(lease) {
            return Err(conflict("calendar_sync_lease", "calendar sync lease is no longer held"));
        }
        let mut keys = BTreeSet::new();
        for event in events {
            if event.uid.trim().is_empty() || event.ends_at < event.starts_at || !keys.insert((&event.uid, &event.recurrence_id)) {
                return Err(invalid("calendar_event", "invalid event UID, time range or duplicate occurrence"));
            }
        }
        let payload = Json(events);
        sqlx::query(
            "insert into calendar_events as e
                (owner,source_id,visibility,uid,recurrence_id,title,description,location,organizer,
                 attendees_json,starts_at,ends_at,timezone,all_day,status,rrule,etag)
             select $1,$2,$3,uid,recurrence_id,title,description,location,organizer,attendees,
                 starts_at,ends_at,timezone,all_day,status,rrule,etag
             from jsonb_to_recordset($4) as x(uid text,recurrence_id text,title text,description text,location text,
                 organizer text,attendees jsonb,starts_at timestamptz,ends_at timestamptz,timezone text,
                 all_day boolean,status text,rrule text,etag text)
             on conflict (source_id,uid,recurrence_id) do update set title=excluded.title,description=excluded.description,
                 location=excluded.location,organizer=excluded.organizer,attendees_json=excluded.attendees_json,
                 starts_at=excluded.starts_at,ends_at=excluded.ends_at,timezone=excluded.timezone,all_day=excluded.all_day,
                 status=excluded.status,rrule=excluded.rrule,etag=excluded.etag,updated_at=now()
             where e.owner=$1",
        )
        .bind(owner)
        .bind(id)
        .bind(visibility)
        .bind(payload)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "delete from calendar_events e where owner=$1 and source_id=$2 and not exists
                (select 1 from jsonb_to_recordset($3) as x(uid text,recurrence_id text)
                 where (x.uid,x.recurrence_id)=(e.uid,e.recurrence_id))",
        )
        .bind(owner)
        .bind(id)
        .bind(payload)
        .execute(&mut *tx)
        .await?;
        let (event_count, last_sync_at) = sqlx::query_as(
            "update calendar_sources set status='ok',sync_etag=$3,sync_leased_until=null,last_sync_at=now(),
                 last_error=null,event_count=$4,updated_at=now()
             where owner=$1 and id=$2 returning event_count,last_sync_at",
        )
        .bind(owner)
        .bind(id)
        .bind(etag)
        .bind(events.len() as i64)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(CalendarSyncResult { event_count, last_sync_at })
    }

    /// Store a sanitized diagnostic; never mutate events, ETag or last successful sync.
    pub async fn record_calendar_sync_error(&self, actor: &Actor, owner: &str, id: Uuid, lease: DateTime<Utc>, error: &str) -> DResult<()> {
        self.finish_calendar_sync(actor, owner, id, lease, false, Some(error)).await
    }

    /// Release a non-replacing sync: no credentials, a fetch error, or HTTP 304.
    pub async fn finish_calendar_sync(
        &self,
        actor: &Actor,
        owner: &str,
        id: Uuid,
        lease: DateTime<Utc>,
        unchanged: bool,
        error: Option<&str>,
    ) -> DResult<()> {
        let owner = Self::sync_owner(actor, owner)?;
        let mut tx = self.db.begin().await?;
        let (_, active, expiry) = calendar_lease(&mut tx, owner, id).await?;
        if !active || expiry != Some(lease) {
            return Err(conflict("calendar_sync_lease", "calendar sync lease is no longer held"));
        }
        sqlx::query(
            "update calendar_sources set status=$3,last_error=$4,sync_leased_until=null,updated_at=now(),
             last_sync_at=case when $3='ok' then now() else last_sync_at end where owner=$1 and id=$2",
        )
        .bind(owner)
        .bind(id)
        .bind(if unchanged {
            "ok"
        } else if error.is_some() {
            "error"
        } else {
            "awaiting_credentials"
        })
        .bind(error.map(|e| crate::mail_bi::bounded_snippet(e, "")))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn list_calendar_events(&self, actor: &Actor, from: DateTime<Utc>, to: DateTime<Utc>) -> DResult<Vec<CalendarEvent>> {
        let owner = self.resolve_owner(actor)?;
        if from >= to || to - from > TimeDelta::days(366) {
            return Err(invalid("calendar_window", "provide a positive calendar window of at most 366 days"));
        }
        let rows: Vec<CalendarEvent> = decode(
            sqlx::query_scalar(
                "select to_jsonb(e) || jsonb_build_object('attendees',attendees_json) from calendar_events e
             where (owner=$1 or visibility='shared') and (
                 (starts_at<$3 and (ends_at>$2 or (ends_at=starts_at and starts_at>=$2)))
                 or (starts_at<$3 and rrule is not null and recurrence_id='')
                 or (recurrence_id<>'' and exists (
                     select 1 from calendar_events base where base.source_id=e.source_id and base.owner=e.owner
                         and base.uid=e.uid and base.recurrence_id='' and base.rrule is not null and base.starts_at<$3
                 ))
             ) limit 5001",
            )
            .bind(owner)
            .bind(from)
            .bind(to)
            .fetch_all(&self.db)
            .await?,
        )?;
        if rows.len() > 5000 {
            return Err(too_many_events());
        }
        expand_events(rows, from, to)
    }

    pub async fn calendar_agenda(&self, actor: &Actor, from: DateTime<Utc>, to: DateTime<Utc>) -> DResult<CalendarAgendaResponse> {
        Ok(CalendarAgendaResponse { from, to, events: self.list_calendar_events(actor, from, to).await? })
    }
}

async fn calendar_lease(conn: &mut PgConnection, owner: &str, id: Uuid) -> DResult<(String, bool, Option<DateTime<Utc>>)> {
    sqlx::query_as(
        "select visibility, enabled and coalesce(sync_leased_until>clock_timestamp(),false), sync_leased_until
         from calendar_sources where owner=$1 and id=$2 for update",
    )
    .bind(owner)
    .bind(id)
    .fetch_optional(conn)
    .await?
    .ok_or(DomainError::NotFound("calendar source"))
}

fn too_many_events() -> DomainError {
    invalid("calendar_window", "calendar window has too many events; request a narrower range")
}

fn overlaps(start: DateTime<Utc>, end: DateTime<Utc>, from: DateTime<Utc>, to: DateTime<Utc>) -> bool {
    start < to && (end > from || (end == start && start >= from))
}

fn expand_events(rows: Vec<CalendarEvent>, from: DateTime<Utc>, to: DateTime<Utc>) -> DResult<Vec<CalendarEvent>> {
    let exceptions: BTreeSet<_> = rows
        .iter()
        .filter(|row| !row.event.recurrence_id.is_empty())
        .filter_map(|row| {
            parse_recurrence_time(&row.event.recurrence_id, row.event.starts_at)
                .map(|original| (row.source_id, row.event.uid.as_str(), original))
        })
        .collect();
    let mut result = Vec::new();
    for row in &rows {
        let event = &row.event;
        if event.status.eq_ignore_ascii_case("cancelled") {
            continue;
        }
        let rule = event.recurrence_id.is_empty().then(|| event.rrule.as_deref().and_then(Rule::parse)).flatten();
        if let Some(rule) = rule {
            let duration = event.ends_at - event.starts_at;
            for start in rule.starts(event.starts_at, duration, from, to)? {
                if exceptions.contains(&(row.source_id, event.uid.as_str(), start)) {
                    continue;
                }
                let mut occurrence = row.clone();
                occurrence.event.starts_at = start;
                occurrence.event.ends_at = start + duration;
                occurrence.event.recurrence_id = start.to_rfc3339();
                result.push(occurrence);
            }
        } else if overlaps(event.starts_at, event.ends_at, from, to)
            && (!event.recurrence_id.is_empty() || !exceptions.contains(&(row.source_id, event.uid.as_str(), event.starts_at)))
        {
            result.push(row.clone());
        }
        if result.len() > 5000 {
            return Err(too_many_events());
        }
    }
    result.sort_by(|a, b| {
        (a.event.starts_at, a.source_id, a.id, &a.event.recurrence_id).cmp(&(b.event.starts_at, b.source_id, b.id, &b.event.recurrence_id))
    });
    Ok(result)
}

fn parse_recurrence_time(value: &str, anchor: DateTime<Utc>) -> Option<DateTime<Utc>> {
    value
        .parse()
        .ok()
        .or_else(|| NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%SZ").ok().map(|date| date.and_utc()))
        .or_else(|| NaiveDate::parse_from_str(value, "%Y%m%d").ok().map(|date| date.and_time(anchor.time()).and_utc()))
}

#[derive(Clone, Copy)]
enum Frequency {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

struct Rule<'a> {
    freq: Frequency,
    interval: u32,
    count: Option<u32>,
    until: Option<&'a str>,
}

impl<'a> Rule<'a> {
    // ponytail: UTC-anchored FREQ/INTERVAL/COUNT/UNTIL only. Unsupported
    // modifiers leave the stored event visible without inventing occurrences.
    // Use a full RFC 5545 engine for BY* rules and local-time DST semantics.
    fn parse(raw: &'a str) -> Option<Self> {
        let mut freq = None;
        let mut interval = None;
        let mut count = None;
        let mut until = None;
        for field in raw.strip_prefix("RRULE:").unwrap_or(raw).split(';') {
            let (name, value) = field.split_once('=')?;
            match name {
                "FREQ" if freq.is_none() => {
                    freq = Some(match value {
                        "DAILY" => Frequency::Daily,
                        "WEEKLY" => Frequency::Weekly,
                        "MONTHLY" => Frequency::Monthly,
                        "YEARLY" => Frequency::Yearly,
                        _ => return None,
                    })
                }
                "INTERVAL" if interval.is_none() => interval = Some(value.parse::<u32>().ok().filter(|&n| n > 0)?),
                "COUNT" if count.is_none() => count = Some(value.parse::<u32>().ok().filter(|&n| n > 0)?),
                "UNTIL" if until.is_none() => {
                    parse_recurrence_time(value, DateTime::UNIX_EPOCH)?;
                    until = Some(value);
                }
                _ => return None,
            }
        }
        if count.is_some() && until.is_some() {
            return None;
        }
        Some(Self { freq: freq?, interval: interval.unwrap_or(1), count, until })
    }

    fn starts(&self, anchor: DateTime<Utc>, duration: TimeDelta, from: DateTime<Utc>, to: DateTime<Utc>) -> DResult<Vec<DateTime<Utc>>> {
        let until = self.until.and_then(|until| parse_recurrence_time(until, anchor));
        let earliest = from.checked_sub_signed(duration).unwrap_or(DateTime::<Utc>::MIN_UTC);
        let elapsed_days = (earliest - anchor).num_days().max(0);
        let elapsed_months =
            ((i64::from(earliest.year()) - i64::from(anchor.year())) * 12 + i64::from(earliest.month()) - i64::from(anchor.month())).max(0);
        let interval = i64::from(self.interval);
        let offset = match self.freq {
            Frequency::Daily => elapsed_days / interval,
            Frequency::Weekly => elapsed_days / (interval * 7),
            Frequency::Monthly if self.count.is_none() => elapsed_months / interval,
            Frequency::Yearly if self.count.is_none() => elapsed_months / (interval * 12),
            _ => 0,
        }
        .saturating_sub(1)
        .max(0) as u64;
        let mut valid = if matches!(self.freq, Frequency::Daily | Frequency::Weekly) { offset } else { 0 };
        let mut starts = Vec::new();
        for index in offset..offset + 10_000 {
            if self.count.is_some_and(|count| valid >= u64::from(count)) {
                return Ok(starts);
            }
            let step = index * u64::from(self.interval);
            let next = match self.freq {
                Frequency::Daily => anchor.checked_add_days(Days::new(step)),
                Frequency::Weekly => anchor.checked_add_days(Days::new(step * 7)),
                Frequency::Monthly | Frequency::Yearly => {
                    let months = if matches!(self.freq, Frequency::Yearly) { step * 12 } else { step };
                    u32::try_from(months).ok().and_then(|months| anchor.checked_add_months(Months::new(months)))
                }
            };
            let Some(next) = next else {
                return Ok(starts);
            };
            if next >= to || until.is_some_and(|until| next > until) {
                return Ok(starts);
            }
            if next.day() != anchor.day() && matches!(self.freq, Frequency::Monthly | Frequency::Yearly) {
                continue;
            }
            valid += 1;
            if next.checked_add_signed(duration).is_some_and(|end| overlaps(next, end, from, to)) {
                starts.push(next);
            }
            if starts.len() > 5000 {
                return Err(too_many_events());
            }
        }
        Err(too_many_events())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn time(iso: &str) -> DateTime<Utc> {
        iso.parse().unwrap()
    }

    fn occurrences(start: &str, rule: &str, from: &str, to: &str) -> Vec<DateTime<Utc>> {
        let start = time(start);
        let rule = Rule::parse(rule).unwrap();
        rule.starts(start, TimeDelta::hours(1), time(from), time(to)).unwrap()
    }

    #[test]
    fn recurrence_respects_frequency_interval_window_and_count() {
        let start = "2026-01-01T10:00:00Z";
        let from = "2026-01-03T10:30:00Z"; // Overlaps the first occurrence in the window.
        let to = "2026-02-01T10:00:00Z";
        assert_eq!(
            occurrences(start, "FREQ=DAILY;INTERVAL=2;COUNT=3", from, to),
            [time("2026-01-03T10:00:00Z"), time("2026-01-05T10:00:00Z")]
        );
        assert_eq!(
            occurrences(start, "RRULE:FREQ=WEEKLY;UNTIL=20260115T100000Z", from, to),
            [time("2026-01-08T10:00:00Z"), time("2026-01-15T10:00:00Z")]
        );
    }

    #[test]
    fn recurrence_skips_invalid_month_days_and_leap_years_without_drift() {
        assert_eq!(
            occurrences("2026-01-31T10:00:00Z", "FREQ=MONTHLY;COUNT=3", "2026-02-01T00:00:00Z", "2026-06-01T00:00:00Z"),
            [time("2026-03-31T10:00:00Z"), time("2026-05-31T10:00:00Z")]
        );
        assert_eq!(
            occurrences("2024-02-29T10:00:00Z", "FREQ=YEARLY;COUNT=2", "2025-01-01T00:00:00Z", "2029-01-01T00:00:00Z"),
            [time("2028-02-29T10:00:00Z")]
        );
    }

    #[test]
    fn recurrence_jumps_to_recent_window_and_ignores_unsupported_rules() {
        assert_eq!(
            occurrences("1900-01-01T10:00:00Z", "FREQ=DAILY", "2026-01-01T00:00:00Z", "2026-01-02T00:00:00Z"),
            [time("2026-01-01T10:00:00Z")]
        );
        assert!(Rule::parse("FREQ=DAILY;INTERVAL=0").is_none());
        assert!(Rule::parse("FREQ=MONTHLY;BYDAY=-1FR").is_none());
    }
}
