CREATE TABLE mail_accounts (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    owner TEXT NOT NULL,
    label TEXT NOT NULL,
    provider TEXT NOT NULL CHECK (provider IN ('gmail', 'protonmail', 'tutanota', 'imap', 'export_dir')),
    transport TEXT NOT NULL CHECK (transport IN ('imap', 'export_dir', 'client_push')),
    address TEXT NOT NULL DEFAULT '',
    config_json JSONB NOT NULL DEFAULT '{}'::jsonb CHECK (
        jsonb_typeof(config_json) = 'object'
        AND config_json - ARRAY['host', 'port', 'tls_mode', 'folders', 'export_dir'] = '{}'::jsonb
    ),
    enabled BOOLEAN NOT NULL DEFAULT true,
    status TEXT NOT NULL DEFAULT 'idle' CHECK (status IN ('idle', 'ok', 'awaiting_credentials', 'error', 'disabled')),
    sync_cursor_json JSONB NOT NULL DEFAULT '{}'::jsonb,
    sync_leased_until TIMESTAMPTZ,
    sync_leased_by TEXT,
    last_sync_at TIMESTAMPTZ,
    last_error TEXT,
    message_count BIGINT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (owner, label),
    UNIQUE (owner, id)
);

CREATE TABLE mail_messages (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id UUID NOT NULL,
    owner TEXT NOT NULL,
    message_id_header TEXT,
    content_sha256 TEXT NOT NULL,
    thread_key TEXT NOT NULL,
    folder TEXT NOT NULL DEFAULT 'INBOX',
    from_addr TEXT NOT NULL DEFAULT '',
    from_name TEXT NOT NULL DEFAULT '',
    from_domain TEXT NOT NULL DEFAULT '',
    to_addrs_json JSONB NOT NULL DEFAULT '[]'::jsonb,
    cc_addrs_json JSONB NOT NULL DEFAULT '[]'::jsonb,
    subject TEXT NOT NULL DEFAULT '',
    sent_at TIMESTAMPTZ,
    received_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    snippet TEXT NOT NULL DEFAULT '',
    body_text TEXT NOT NULL DEFAULT '',
    has_attachments BOOLEAN NOT NULL DEFAULT false,
    attachment_names_json JSONB NOT NULL DEFAULT '[]'::jsonb,
    raw_headers_json JSONB NOT NULL DEFAULT '{}'::jsonb,
    size_bytes BIGINT NOT NULL DEFAULT 0,
    is_read BOOLEAN NOT NULL DEFAULT false,
    raw_path TEXT,
    ingested_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (account_id, content_sha256),
    UNIQUE (account_id, id),
    UNIQUE (owner, id),
    FOREIGN KEY (owner, account_id) REFERENCES mail_accounts(owner, id) ON DELETE CASCADE
);

CREATE INDEX mail_messages_received ON mail_messages (owner, received_at DESC);
CREATE INDEX mail_messages_domain ON mail_messages (owner, from_domain);
CREATE INDEX mail_messages_header ON mail_messages (account_id, message_id_header);
CREATE INDEX mail_messages_fts ON mail_messages USING GIN (
    to_tsvector('simple', coalesce(subject,'') || ' ' || coalesce(from_addr,'') || ' ' ||
        coalesce(from_name,'') || ' ' || coalesce(body_text,''))
);

CREATE TABLE mail_imap_locations (
    account_id UUID NOT NULL REFERENCES mail_accounts(id) ON DELETE CASCADE,
    folder TEXT NOT NULL,
    uidvalidity BIGINT NOT NULL,
    uid BIGINT NOT NULL,
    message_id UUID NOT NULL,
    seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, folder, uidvalidity, uid),
    FOREIGN KEY (account_id, message_id) REFERENCES mail_messages(account_id, id) ON DELETE CASCADE
);
CREATE INDEX mail_imap_locations_message ON mail_imap_locations (account_id, message_id);

CREATE TABLE mail_analysis (
    message_id UUID PRIMARY KEY,
    owner TEXT NOT NULL,
    classification TEXT NOT NULL CHECK (classification IN (
        'business', 'financial', 'invoice', 'receipt', 'legal', 'action_required',
        'personal', 'newsletter', 'promo', 'junk', 'automated'
    )),
    is_junk BOOLEAN NOT NULL DEFAULT false,
    junk_score DOUBLE PRECISION NOT NULL DEFAULT 0.0 CHECK (junk_score BETWEEN 0 AND 1),
    priority TEXT NOT NULL DEFAULT 'normal' CHECK (priority IN ('urgent', 'high', 'normal', 'low', 'noise')),
    summary TEXT NOT NULL DEFAULT '',
    entities_json JSONB NOT NULL DEFAULT '{}'::jsonb,
    action_items_json JSONB NOT NULL DEFAULT '[]'::jsonb,
    topics_json JSONB NOT NULL DEFAULT '[]'::jsonb,
    user_override_classification TEXT CHECK (user_override_classification IN (
        'business', 'financial', 'invoice', 'receipt', 'legal', 'action_required',
        'personal', 'newsletter', 'promo', 'junk', 'automated'
    )),
    user_override_junk BOOLEAN,
    analyzer_kind TEXT NOT NULL DEFAULT 'heuristic' CHECK (analyzer_kind IN ('heuristic', 'local_llm')),
    analyzer_model TEXT,
    analyzed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    FOREIGN KEY (owner, message_id) REFERENCES mail_messages(owner, id) ON DELETE CASCADE
);

CREATE INDEX mail_analysis_classification ON mail_analysis (owner, is_junk, classification);

CREATE TABLE mail_bi_digests (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    owner TEXT NOT NULL,
    period_kind TEXT NOT NULL,
    period_start TIMESTAMPTZ NOT NULL,
    period_end TIMESTAMPTZ NOT NULL,
    summary_markdown TEXT NOT NULL,
    metrics_json JSONB NOT NULL DEFAULT '{}'::jsonb,
    highlights_json JSONB NOT NULL DEFAULT '[]'::jsonb,
    generated_by TEXT NOT NULL DEFAULT 'heuristic',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (owner, period_kind, period_start, period_end),
    CHECK (period_end > period_start)
);

CREATE INDEX mail_bi_digests_created ON mail_bi_digests (owner, created_at DESC);

CREATE TABLE calendar_sources (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    owner TEXT NOT NULL,
    label TEXT NOT NULL,
    provider TEXT NOT NULL CHECK (provider IN ('google_ics', 'caldav', 'google_api', 'ics_url')),
    visibility TEXT NOT NULL DEFAULT 'private' CHECK (visibility IN ('private', 'shared')),
    color TEXT NOT NULL DEFAULT '#4f8cff',
    enabled BOOLEAN NOT NULL DEFAULT true,
    status TEXT NOT NULL DEFAULT 'idle',
    sync_etag TEXT,
    sync_leased_until TIMESTAMPTZ,
    last_sync_at TIMESTAMPTZ,
    last_error TEXT,
    event_count BIGINT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (owner, label),
    UNIQUE (owner, id, visibility)
);

CREATE TABLE calendar_events (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    source_id UUID NOT NULL,
    owner TEXT NOT NULL,
    visibility TEXT NOT NULL DEFAULT 'private' CHECK (visibility IN ('private', 'shared')),
    uid TEXT NOT NULL,
    recurrence_id TEXT NOT NULL DEFAULT '',
    title TEXT NOT NULL DEFAULT '',
    description TEXT NOT NULL DEFAULT '',
    location TEXT NOT NULL DEFAULT '',
    organizer TEXT NOT NULL DEFAULT '',
    attendees_json JSONB NOT NULL DEFAULT '[]'::jsonb,
    starts_at TIMESTAMPTZ NOT NULL,
    ends_at TIMESTAMPTZ NOT NULL,
    timezone TEXT NOT NULL DEFAULT 'UTC',
    all_day BOOLEAN NOT NULL DEFAULT false,
    status TEXT NOT NULL DEFAULT 'confirmed',
    rrule TEXT,
    etag TEXT,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (source_id, uid, recurrence_id),
    CHECK (ends_at >= starts_at),
    FOREIGN KEY (owner, source_id, visibility) REFERENCES calendar_sources(owner, id, visibility)
        ON UPDATE CASCADE ON DELETE CASCADE
);

CREATE INDEX calendar_events_owner_window ON calendar_events (owner, starts_at, ends_at);
CREATE INDEX calendar_events_shared_window ON calendar_events (visibility, starts_at);
