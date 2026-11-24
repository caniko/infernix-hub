use chrono::{DateTime, Utc};
use contracts::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, types::Json};
use uuid::Uuid;

use crate::mail_bi::{CLASSES, analyze_message, bounded_body, bounded_snippet, executive_digest, sender_domain};
use crate::{Actor, DResult, DomainError, Hub, decode, decode_one, invalid};

// `owner` is always resolved before constructing a user query. Composite
// foreign keys also prevent mismatched ownership from being persisted.
const MESSAGE_SUMMARY: &str = "
    m.id,m.account_id,a.provider,m.thread_key,m.folder,m.from_addr,m.from_name,m.from_domain,
    m.subject,m.sent_at,m.received_at,m.snippet,m.has_attachments,m.is_read,
    coalesce(n.user_override_classification,n.classification) as classification,
    coalesce(n.user_override_junk,n.is_junk) as is_junk,n.priority";

const MESSAGE_DETAIL: &str = "
           m.message_id_header,m.content_sha256,m.body_text,m.size_bytes,m.ingested_at,
           m.to_addrs_json as to_addrs,m.cc_addrs_json as cc_addrs,
           m.attachment_names_json as attachment_names, m.raw_headers_json as raw_headers,
           to_jsonb(n) || jsonb_build_object('entities', n.entities_json,
               'action_items', n.action_items_json, 'topics', n.topics_json) as analysis";

const MESSAGE_SCOPE: &str = "
    from mail_messages m
    join mail_accounts a on (a.owner,a.id) = (m.owner,m.account_id)
    join mail_analysis n on (n.owner,n.message_id) = (m.owner,m.id)
    where m.owner = $1";

fn require_class(class: &str) -> DResult<()> {
    if CLASSES.contains(&class) { Ok(()) } else { Err(invalid("mail_classification", "invalid mail classification")) }
}

impl Hub {
    pub async fn upsert_mail_account_system(&self, actor: &Actor, owner: &str, input: &MailAccountUpsert) -> DResult<MailAccount> {
        let owner = Self::sync_owner(actor, owner)?;
        if input.label.trim().is_empty()
            || !matches!(input.provider.as_str(), "gmail" | "protonmail" | "tutanota" | "imap" | "export_dir")
            || !matches!(input.transport.as_str(), "imap" | "export_dir" | "client_push")
        {
            return Err(invalid("mail_account", "invalid account label, provider or transport"));
        }
        decode_one(
            sqlx::query_scalar(
                "insert into mail_accounts as a (owner,label,provider,transport,address,config_json,enabled,status)
             values ($1,$2,$3,$4,$5,$6,$7,case when $7 then 'idle' else 'disabled' end)
             on conflict (owner,label) do update set provider=excluded.provider, transport=excluded.transport,
               address=excluded.address, config_json=excluded.config_json, enabled=excluded.enabled,
               status=case when not excluded.enabled then 'disabled' when not a.enabled then 'idle' else a.status end,
               sync_leased_by=case when excluded.enabled then a.sync_leased_by end,
               sync_leased_until=case when excluded.enabled then a.sync_leased_until end, updated_at=now()
             returning to_jsonb(a)",
            )
            .bind(owner)
            .bind(&input.label)
            .bind(&input.provider)
            .bind(&input.transport)
            .bind(&input.address)
            .bind(Json(&input.config_json))
            .bind(input.enabled)
            .fetch_one(&self.db)
            .await?,
        )
    }

    pub async fn list_mail_accounts(&self, actor: &Actor) -> DResult<Vec<MailAccount>> {
        let owner = self.resolve_owner(actor)?;
        decode(
            sqlx::query_scalar("select to_jsonb(a) from mail_accounts a where owner=$1 order by label")
                .bind(owner)
                .fetch_all(&self.db)
                .await?,
        )
    }

    pub async fn mail_account_system(&self, actor: &Actor, owner: &str, label: &str) -> DResult<MailAccount> {
        let owner = Self::sync_owner(actor, owner)?;
        let row = sqlx::query_scalar("select to_jsonb(a) from mail_accounts a where owner=$1 and label=$2")
            .bind(owner)
            .bind(label)
            .fetch_optional(&self.db)
            .await?
            .ok_or(DomainError::NotFound("mail account"))?;
        decode_one(row)
    }

    /// Returns a unique token for this acquisition, not a stable worker ID.
    pub async fn acquire_mail_sync_lease(&self, actor: &Actor, owner: &str, id: Uuid, seconds: u32) -> DResult<Option<Uuid>> {
        let owner = Self::sync_owner(actor, owner)?;
        let token = Uuid::new_v4();
        let acquired = sqlx::query(
            "update mail_accounts set sync_leased_by=$3, sync_leased_until=now()+make_interval(secs=>$4)
             where owner=$1 and id=$2 and enabled and (sync_leased_until is null or sync_leased_until<=now())",
        )
        .bind(owner)
        .bind(id)
        .bind(token.to_string())
        .bind(f64::from(seconds.clamp(1, 3600)))
        .execute(&self.db)
        .await?;
        if acquired.rows_affected() == 1 {
            Ok(Some(token))
        } else {
            self.require_mail_account(owner, id).await?;
            Ok(None)
        }
    }

    pub async fn release_mail_sync_lease(&self, actor: &Actor, owner: &str, id: Uuid, token: Uuid) -> DResult<()> {
        let owner = Self::sync_owner(actor, owner)?;
        let result = sqlx::query(
            "update mail_accounts set sync_leased_until=null, sync_leased_by=null where owner=$1 and id=$2 and sync_leased_by=$3",
        )
        .bind(owner)
        .bind(id)
        .bind(token.to_string())
        .execute(&self.db)
        .await?;
        if result.rows_affected() == 0 {
            self.require_mail_account(owner, id).await?;
        }
        Ok(())
    }

    /// Store only a sanitized error string, never a provider response or credential.
    pub async fn update_mail_account_sync_state(
        &self,
        actor: &Actor,
        owner: &str,
        id: Uuid,
        token: Uuid,
        status: &str,
        cursor: Option<&Value>,
        error: Option<&str>,
    ) -> DResult<MailAccount> {
        let owner = Self::sync_owner(actor, owner)?;
        if !matches!(status, "idle" | "ok" | "awaiting_credentials" | "error" | "disabled") {
            return Err(invalid("mail_sync_status", "invalid account sync status"));
        }
        let row = sqlx::query_scalar(
            "update mail_accounts a set status=$4, sync_cursor_json=coalesce($5,sync_cursor_json),
               last_error=$6, last_sync_at=case when $4='ok' then now() else last_sync_at end, updated_at=now()
             where owner=$1 and id=$2 and sync_leased_by=$3 and sync_leased_until>now() returning to_jsonb(a)",
        )
        .bind(owner)
        .bind(id)
        .bind(token.to_string())
        .bind(status)
        .bind(cursor)
        .bind(error.map(|error| bounded_snippet(error, "")))
        .fetch_optional(&self.db)
        .await?;
        match row {
            Some(row) => decode_one(row),
            None => {
                self.require_mail_account(owner, id).await?;
                Err(crate::conflict("mail_sync_lease", "mail sync lease is no longer held"))
            }
        }
    }

    async fn require_mail_account(&self, owner: &str, id: Uuid) -> DResult<()> {
        sqlx::query_scalar::<_, Uuid>("select id from mail_accounts where owner=$1 and id=$2")
            .bind(owner)
            .bind(id)
            .fetch_optional(&self.db)
            .await?
            .ok_or(DomainError::NotFound("mail account"))?;
        Ok(())
    }

    pub async fn ingest_mail_messages(&self, actor: &Actor, request: &MailIngestRequest) -> DResult<MailIngestResult> {
        self.ingest_mail_for_owner(&self.resolve_owner(actor)?, None, request).await
    }

    pub async fn ingest_named_mail_messages(&self, actor: &Actor, input: MailNamedIngestRequest) -> DResult<MailIngestResult> {
        let owner = self.resolve_owner(actor)?;
        let account = input.account.trim();
        let address = input.address.as_deref().unwrap_or_default();
        let provider = input.provider.as_deref().unwrap_or("export_dir");
        if account.is_empty()
            || account.len() > 128
            || address.len() > 512
            || account.chars().chain(address.chars()).any(char::is_control)
            || !matches!(provider, "gmail" | "protonmail" | "tutanota" | "imap" | "export_dir")
            || input.messages.len() > 100
        {
            return Err(invalid("mail_ingest", "invalid account metadata or batch size"));
        }
        // A client can create its own push account, but cannot reconfigure one.
        sqlx::query(
            "insert into mail_accounts (owner,label,provider,transport,address)
            values ($1,$2,$3,'client_push',$4) on conflict (owner,label) do nothing",
        )
        .bind(&owner)
        .bind(account)
        .bind(provider)
        .bind(address)
        .execute(&self.db)
        .await?;
        let account_id = sqlx::query_scalar("select id from mail_accounts where owner=$1 and label=$2")
            .bind(&owner)
            .bind(account)
            .fetch_one(&self.db)
            .await?;
        self.ingest_mail_for_owner(&owner, None, &MailIngestRequest { account_id, messages: input.messages }).await
    }

    pub async fn ingest_mail_messages_system(
        &self,
        actor: &Actor,
        owner: &str,
        token: Uuid,
        request: &MailIngestRequest,
    ) -> DResult<MailIngestResult> {
        self.ingest_mail_for_owner(Self::sync_owner(actor, owner)?, Some(token), request).await
    }

    async fn ingest_mail_for_owner(&self, owner: &str, token: Option<Uuid>, request: &MailIngestRequest) -> DResult<MailIngestResult> {
        if request.messages.len() > 100 {
            return Err(invalid("mail_batch_size", "a mail batch may contain at most 100 messages"));
        }
        let account = request.account_id;
        let mut tx = self.db.begin().await?;
        let (enabled, held, lease_valid): (bool, Option<String>, bool) = sqlx::query_as(
            "select enabled,sync_leased_by,coalesce(sync_leased_until>now(),false)
             from mail_accounts where owner=$1 and id=$2 for update",
        )
        .bind(owner)
        .bind(account)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(DomainError::NotFound("mail account"))?;
        if !enabled {
            return Err(crate::conflict("mail_account_disabled", "mail account is disabled"));
        }
        if let Some(token) = token {
            if !lease_valid || held.as_deref() != Some(token.to_string().as_str()) {
                return Err(crate::conflict("mail_sync_lease", "mail sync lease is no longer held"));
            }
        }
        let mut inserted = 0;
        let mut junk = 0;
        for message in &request.messages {
            let sha = content_hash(message)?;
            let body = bounded_body(&message.body_text);
            let snippet = bounded_snippet(&message.snippet, body);
            let thread =
                if message.thread_key.is_empty() { message.message_id_header.as_deref().unwrap_or(&sha) } else { &message.thread_key };
            let folder = if message.folder.is_empty() {
                message.imap_location.as_ref().map_or("INBOX", |location| location.folder.as_str())
            } else {
                &message.folder
            };
            let id: Option<Uuid> = sqlx::query_scalar(
                "insert into mail_messages
                   (owner,account_id,message_id_header,content_sha256,thread_key,folder,from_addr,from_name,from_domain,
                    to_addrs_json,cc_addrs_json,subject,sent_at,received_at,snippet,body_text,has_attachments,
                    attachment_names_json,raw_headers_json,size_bytes,is_read,raw_path)
                 values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,coalesce($14,now()),$15,$16,$17,$18,$19,$20,$21,$22)
                 on conflict (account_id,content_sha256) do nothing returning id",
            )
            .bind(owner)
            .bind(account)
            .bind(&message.message_id_header)
            .bind(&sha)
            .bind(thread)
            .bind(folder)
            .bind(&message.from_addr)
            .bind(&message.from_name)
            .bind(sender_domain(&message.from_addr))
            .bind(Json(&message.to_addrs))
            .bind(Json(&message.cc_addrs))
            .bind(&message.subject)
            .bind(message.sent_at)
            .bind(message.received_at.or(message.sent_at))
            .bind(snippet)
            .bind(body)
            .bind(message.has_attachments)
            .bind(Json(&message.attachment_names))
            .bind(Json(&message.raw_headers))
            .bind(message.size_bytes)
            .bind(message.is_read)
            .bind(message.raw_path.as_deref().filter(|_| token.is_some()))
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(id) = id {
                let analysis = analyze_message(message, Utc::now());
                junk += usize::from(analysis.is_junk);
                save_analysis(&mut tx, owner, id, &analysis, None).await?;
                inserted += 1;
            }
            if let Some(location) = &message.imap_location {
                if location.folder.is_empty()
                    || !(1..=i64::from(u32::MAX)).contains(&location.uid)
                    || !(1..=i64::from(u32::MAX)).contains(&location.uidvalidity)
                {
                    return Err(invalid("mail_imap_location", "invalid IMAP folder, UID or UIDVALIDITY"));
                }
                sqlx::query(
                    "insert into mail_imap_locations as l (account_id,folder,uidvalidity,uid,message_id)
                     select account_id,$4,$5,$6,id from mail_messages where owner=$1 and account_id=$2 and content_sha256=$3
                     on conflict (account_id,folder,uidvalidity,uid) do update set message_id=excluded.message_id, seen_at=now()
                     where exists (select 1 from mail_accounts where owner=$1 and id=l.account_id)",
                )
                .bind(owner)
                .bind(account)
                .bind(&sha)
                .bind(&location.folder)
                .bind(location.uidvalidity)
                .bind(location.uid)
                .execute(&mut *tx)
                .await?;
            }
        }
        sqlx::query("update mail_accounts set message_count=message_count+$3, updated_at=now() where owner=$1 and id=$2")
            .bind(owner)
            .bind(account)
            .bind(inserted as i64)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(MailIngestResult { inserted, duplicates: request.messages.len() - inserted, junk })
    }

    pub async fn list_mail_messages(&self, actor: &Actor, query: &MailSearchQuery) -> DResult<Vec<MailMessageSummary>> {
        let owner = self.resolve_owner(actor)?;
        if let Some(id) = query.account_id {
            self.require_mail_account(&owner, id).await?;
        }
        if let Some(class) = &query.classification {
            require_class(class)?;
        }
        let q = query.q.as_deref().map(str::trim).filter(|q| !q.is_empty());
        if q.is_some_and(|q| q.len() > 1024) {
            return Err(invalid("mail_search", "mail search query is too long"));
        }
        let sql = format!(
            "select to_jsonb(t) from (select {MESSAGE_SUMMARY} {MESSAGE_SCOPE}
             and ($2::uuid is null or m.account_id=$2) and ($3::text is null or a.provider=$3)
             and ($4::text is null or coalesce(n.user_override_classification,n.classification)=$4)
             and ($5 or not coalesce(n.user_override_junk,n.is_junk))
             and ($6::text is null
                or to_tsvector('simple',coalesce(m.subject,'') || ' ' || coalesce(m.from_addr,'') || ' ' ||
                     coalesce(m.from_name,'') || ' ' || coalesce(m.body_text,'')) @@ websearch_to_tsquery('simple',$6)
                or m.subject ilike $7 escape '!' or m.from_addr ilike $7 escape '!'
                or m.from_name ilike $7 escape '!' or m.body_text ilike $7 escape '!')
             order by m.received_at desc,m.id limit $8 offset $9) t",
        );
        let rows = sqlx::query_scalar(&sql)
            .bind(owner)
            .bind(query.account_id)
            .bind(&query.provider)
            .bind(&query.classification)
            .bind(query.include_junk)
            .bind(q)
            .bind(q.map(|q| format!("%{}%", q.replace('!', "!!").replace('%', "!%").replace('_', "!_"))))
            .bind(i64::from(query.limit.unwrap_or(50).clamp(1, 100)))
            .bind(i64::from(query.offset.unwrap_or_default()))
            .fetch_all(&self.db)
            .await?;
        decode(rows)
    }

    pub async fn get_mail_message(&self, actor: &Actor, id: Uuid) -> DResult<MailMessageDetail> {
        let owner = self.resolve_owner(actor)?;
        let mut conn = self.db.acquire().await?;
        mail_detail(&mut conn, &owner, id).await
    }

    pub async fn override_mail_triage(&self, actor: &Actor, id: Uuid, input: &MailTriageOverride) -> DResult<MailMessageDetail> {
        let owner = self.resolve_owner(actor)?;
        if let Some(Some(class)) = &input.classification {
            require_class(class)?;
        }
        let mut tx = self.db.begin().await?;
        sqlx::query(
            "update mail_analysis set user_override_classification=case when $3 then $4 else user_override_classification end,
               user_override_junk=case when $5 then $6 else user_override_junk end where owner=$1 and message_id=$2",
        )
        .bind(&owner)
        .bind(id)
        .bind(input.classification.is_some())
        .bind(input.classification.as_ref().and_then(Option::as_deref))
        .bind(input.is_junk.is_some())
        .bind(input.is_junk.flatten())
        .execute(&mut *tx)
        .await?;
        let detail = mail_detail(&mut tx, &owner, id).await?;
        tx.commit().await?;
        Ok(detail)
    }

    pub async fn list_unenriched_mail(&self, actor: &Actor, owner: &str, limit: u32, skip_junk: bool) -> DResult<Vec<MailMessageDetail>> {
        let owner = Self::sync_owner(actor, owner)?;
        decode(
            sqlx::query_scalar(&format!(
                "select to_jsonb(t) from (select {MESSAGE_SUMMARY},{MESSAGE_DETAIL} {MESSAGE_SCOPE}
             and (not $3 or not coalesce(n.user_override_junk,n.is_junk)) and n.analyzer_kind='heuristic'
             order by m.received_at desc,m.id limit $2) t",
            ))
            .bind(owner)
            .bind(i64::from(limit.clamp(1, 100)))
            .bind(skip_junk)
            .fetch_all(&self.db)
            .await?,
        )
    }

    pub async fn mail_owners_system(&self, actor: &Actor) -> DResult<Vec<String>> {
        Self::sync_owner(actor, "worker")?;
        Ok(sqlx::query_scalar("select distinct owner from mail_accounts order by owner").fetch_all(&self.db).await?)
    }

    pub async fn save_llm_analysis(&self, actor: &Actor, owner: &str, id: Uuid, model: &str, analysis: &MailAnalysisView) -> DResult<()> {
        let owner = Self::sync_owner(actor, owner)?;
        require_class(&analysis.classification)?;
        if !matches!(analysis.priority.as_str(), "urgent" | "high" | "normal" | "low" | "noise")
            || !(0.0..=1.0).contains(&analysis.junk_score)
            || model.trim().is_empty()
        {
            return Err(invalid("mail_analysis", "invalid analysis priority, junk score or model"));
        }
        let mut conn = self.db.acquire().await?;
        save_analysis(&mut conn, owner, id, analysis, Some(model)).await
    }

    pub async fn mail_bi_overview(&self, actor: &Actor) -> DResult<MailBiOverview> {
        self.mail_overview_for_owner(&self.resolve_owner(actor)?, None, None).await
    }

    async fn mail_overview_for_owner(
        &self,
        owner: &str,
        from: Option<DateTime<Utc>>,
        to: Option<DateTime<Utc>>,
    ) -> DResult<MailBiOverview> {
        decode_one(sqlx::query_scalar(
            "with scoped as (
                select m.id,m.subject,m.received_at,m.is_read,m.from_domain,m.from_addr,a.provider,
                       coalesce(n.user_override_classification,n.classification) as classification,
                       coalesce(n.user_override_junk,n.is_junk) as junk,n.entities_json as entities,n.action_items_json as actions
                from mail_messages m
                join mail_accounts a on (a.owner,a.id)=(m.owner,m.account_id)
                join mail_analysis n on (n.owner,n.message_id)=(m.owner,m.id)
                where m.owner=$1 and ($2::timestamptz is null or m.received_at >= $2)
                  and ($3::timestamptz is null or m.received_at < $3)
             ), business as (
                select * from scoped where not junk and classification in ('business','financial','invoice','receipt','legal','action_required')
             ), financial as (
                select * from business where classification in ('financial','invoice','receipt')
             ), vendors as (
                select entities->>'vendor' as vendor,count(*) as message_count from financial
                where nullif(entities->>'vendor','') is not null group by 1 having count(*)>=2
             )
             select jsonb_build_object(
                'total_count',count(*), 'signal_count',count(*) filter(where not junk),
                'junk_count',count(*) filter(where junk), 'unread_count',count(*) filter(where not is_read),
                'unread_signal_count',count(*) filter(where not is_read and not junk),
                'recurring_vendor_count',(select count(*) from vendors),
                'open_action_item_count',coalesce(sum(jsonb_array_length(actions)) filter(where not junk),0),
                'by_provider',coalesce((select jsonb_object_agg(provider,n) from
                    (select provider,count(*) as n from scoped group by provider) x),'{}'),
                'by_classification',coalesce((select jsonb_object_agg(classification,n) from
                    (select classification,count(*) as n from scoped group by classification) x),'{}'),
                'business_domains',coalesce((select jsonb_agg(to_jsonb(x) order by message_count desc,key) from
                    (select from_domain as key,count(*) as message_count from business where from_domain<>''
                     group by 1 order by 2 desc,1 limit 10) x),'[]'),
                'business_senders',coalesce((select jsonb_agg(to_jsonb(x) order by message_count desc,key) from
                    (select lower(from_addr) as key,count(*) as message_count from business where from_addr<>''
                     group by 1 order by 2 desc,1 limit 10) x),'[]'),
                'recurring_vendors',coalesce((select jsonb_agg(to_jsonb(x) order by message_count desc,vendor) from
                    (select * from vendors order by message_count desc,vendor limit 10) x),'[]'),
                'recent_amounts',coalesce((select jsonb_agg(amount order by received_at desc,id,ord) from
                    (select amount,received_at,id,ord from financial cross join lateral
                     jsonb_array_elements(entities->'amounts') with ordinality as v(amount,ord)
                     order by received_at desc,id,ord limit 20) x),'[]'),
                'open_action_items',coalesce((select jsonb_agg(jsonb_build_object('message_id',id,'subject',subject,'action',action)
                     order by received_at desc,id,ord) from
                    (select id,subject,received_at,action,ord from scoped cross join lateral
                     jsonb_array_elements_text(actions) with ordinality as v(action,ord)
                     where not junk order by received_at desc,id,ord limit 20) x),'[]'),
                'unsubscribe_candidates',coalesce((select jsonb_agg(to_jsonb(x) order by message_count desc,key) from
                    (select from_domain as key,count(*) as message_count from scoped where junk and from_domain<>''
                     group by 1 order by 2 desc,1 limit 10) x),'[]')
             ) from scoped",
        ).bind(owner).bind(from).bind(to).fetch_one(&self.db).await?)
    }

    pub async fn create_mail_bi_digest(&self, actor: &Actor, kind: &str, from: DateTime<Utc>, to: DateTime<Utc>) -> DResult<MailBiDigest> {
        let owner = self.resolve_owner(actor)?;
        if kind.trim().is_empty() || kind.len() > 64 || from >= to {
            return Err(invalid("mail_digest_period", "invalid digest period or period kind"));
        }
        let overview = self.mail_overview_for_owner(&owner, Some(from), Some(to)).await?;
        let highlights = json!({
            "recurring_vendors": overview.recurring_vendors,
            "open_action_items": overview.open_action_items,
            "unsubscribe_candidates": overview.unsubscribe_candidates,
        });
        decode_one(
            sqlx::query_scalar(
                "insert into mail_bi_digests as d (owner,period_kind,period_start,period_end,summary_markdown,metrics_json,highlights_json)
             values ($1,$2,$3,$4,$5,$6,$7)
             on conflict (owner,period_kind,period_start,period_end) do update set
               summary_markdown=excluded.summary_markdown,metrics_json=excluded.metrics_json,highlights_json=excluded.highlights_json,
               generated_by=excluded.generated_by
             returning to_jsonb(d)",
            )
            .bind(owner)
            .bind(kind)
            .bind(from)
            .bind(to)
            .bind(executive_digest(&overview))
            .bind(Json(&overview))
            .bind(highlights)
            .fetch_one(&self.db)
            .await?,
        )
    }

    pub async fn summarize_mail_bi_digest(&self, actor: &Actor, id: Uuid, model: &str, summary: &str) -> DResult<MailBiDigest> {
        let owner = self.resolve_owner(actor)?;
        let summary = crate::mail_bi::markdown_literal(summary);
        let row = sqlx::query_scalar(
            "update mail_bi_digests d set summary_markdown=$3,generated_by=$4 where owner=$1 and id=$2 returning to_jsonb(d)",
        )
        .bind(owner)
        .bind(id)
        .bind(summary)
        .bind(model)
        .fetch_optional(&self.db)
        .await?
        .ok_or(DomainError::NotFound("mail BI digest"))?;
        decode_one(row)
    }

    pub async fn list_mail_bi_digests(&self, actor: &Actor) -> DResult<Vec<MailBiDigest>> {
        decode(
            sqlx::query_scalar("select to_jsonb(d) from mail_bi_digests d where owner=$1 order by created_at desc,id limit 100")
                .bind(self.resolve_owner(actor)?)
                .fetch_all(&self.db)
                .await?,
        )
    }
}

async fn mail_detail(conn: &mut PgConnection, owner: &str, id: Uuid) -> DResult<MailMessageDetail> {
    decode_one(
        sqlx::query_scalar(&format!("select to_jsonb(t) from (select {MESSAGE_SUMMARY},{MESSAGE_DETAIL} {MESSAGE_SCOPE} and m.id=$2) t"))
            .bind(owner)
            .bind(id)
            .fetch_optional(conn)
            .await?
            .ok_or(DomainError::NotFound("mail message"))?,
    )
}

async fn save_analysis(conn: &mut PgConnection, owner: &str, id: Uuid, a: &MailAnalysisView, model: Option<&str>) -> DResult<()> {
    let result = sqlx::query(
        "insert into mail_analysis as n
           (message_id,owner,classification,is_junk,junk_score,priority,summary,entities_json,action_items_json,topics_json,analyzer_kind,analyzer_model)
         select id,owner,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12 from mail_messages where owner=$1 and id=$2
         on conflict (message_id) do update set classification=excluded.classification,is_junk=excluded.is_junk,
           junk_score=excluded.junk_score,priority=excluded.priority,summary=excluded.summary,entities_json=excluded.entities_json,
           action_items_json=excluded.action_items_json,topics_json=excluded.topics_json,analyzer_kind=excluded.analyzer_kind,
           analyzer_model=excluded.analyzer_model,analyzed_at=now()
         where n.owner=$1",
    ).bind(owner).bind(id).bind(&a.classification).bind(a.is_junk).bind(a.junk_score).bind(&a.priority).bind(bounded_snippet(&a.summary,""))
        .bind(Json(&a.entities)).bind(Json(&a.action_items)).bind(Json(&a.topics))
        .bind(if model.is_some() { "local_llm" } else { "heuristic" }).bind(model).execute(conn).await?;
    if result.rows_affected() == 0 {
        return Err(DomainError::NotFound("mail message"));
    }
    Ok(())
}

fn content_hash(message: &MailMessageIn) -> DResult<String> {
    if let Some(sha) = &message.content_sha256 {
        if sha.len() != 64 || !sha.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err(invalid("mail_content_hash", "content_sha256 must be 64 hexadecimal digits"));
        }
        return Ok(sha.to_ascii_lowercase());
    }
    // Only message content contributes; folder, locator, read-state and import
    // timestamps can differ when the same message is seen again.
    let content = (
        &message.message_id_header,
        &message.from_addr,
        &message.from_name,
        &message.to_addrs,
        &message.cc_addrs,
        &message.subject,
        message.sent_at,
        &message.body_text,
        &message.attachment_names,
        &message.raw_headers,
        message.has_attachments,
        message.size_bytes,
    );
    let mut hash = Sha256::new();
    serde_json::to_writer(&mut hash, &content).map_err(|error| DomainError::Internal(error.to_string()))?;
    Ok(hex::encode(hash.finalize()))
}
