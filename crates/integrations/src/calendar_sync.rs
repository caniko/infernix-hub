//! Snapshot ingestion for iCalendar, CalDAV and Google Calendar.

use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use contracts::CalendarEventIn;
use reqwest::{Client, RequestBuilder, Response, StatusCode, Url};
use ring::{
    rand::SystemRandom,
    signature::{RSA_PKCS1_SHA256, RsaKeyPair},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{borrow::Cow, io};
use tokio::io::AsyncReadExt;

const MAX_BYTES: usize = 32 * 1024 * 1024;
const MAX_EVENTS: usize = 20_000;

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn text(value: &str) -> String {
    let mut output = String::new();
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            output.push(ch);
            continue;
        }
        match chars.next() {
            Some('n' | 'N') => output.push('\n'),
            Some(ch @ ('\\' | ',' | ';')) => output.push(ch),
            Some(ch) => {
                output.push('\\');
                output.push(ch);
            }
            None => output.push('\\'),
        }
    }
    output
}

/// `offset` excludes colons/semicolons inside a quoted property parameter.
fn offset(line: &str, delimiter: char) -> Option<usize> {
    let mut quoted = false;
    line.char_indices().find_map(|(i, ch)| {
        if ch == '"' {
            quoted = !quoted;
        }
        (!quoted && ch == delimiter).then_some(i)
    })
}

fn ical_datetime(value: &str, parameters: &str, default_zone: &str) -> io::Result<(DateTime<Utc>, String, bool)> {
    let mut zone = default_zone;
    let mut date = value.len() == 8;
    let mut parameters = parameters;
    while !parameters.is_empty() {
        let at = offset(parameters, ';').unwrap_or(parameters.len());
        let part = &parameters[..at];
        parameters = parameters.get(at + 1..).unwrap_or_default();
        if let Some((key, val)) = part.split_once('=') {
            let val = val.trim_matches('"');
            if key.eq_ignore_ascii_case("TZID") {
                zone = val;
            }
            if key.eq_ignore_ascii_case("VALUE") {
                date = val.eq_ignore_ascii_case("DATE");
            }
        }
    }
    let malformed = || invalid("invalid calendar date or timezone");
    if date {
        let naive = NaiveDate::parse_from_str(value, "%Y%m%d").map_err(|_| malformed())?.and_hms_opt(0, 0, 0).ok_or_else(malformed)?;
        return Ok((naive.and_utc(), "UTC".into(), true));
    }
    let local = if let Some(value) = value.strip_suffix('Z') {
        zone = "UTC";
        value
    } else {
        value
    };
    let naive = NaiveDateTime::parse_from_str(local, "%Y%m%dT%H%M%S").map_err(|_| malformed())?;
    let timezone = zone.parse::<Tz>().map_err(|_| malformed())?;
    let time = timezone.from_local_datetime(&naive).earliest().ok_or_else(malformed)?;
    Ok((time.with_timezone(&Utc), zone.into(), false))
}

fn ical_duration(value: &str) -> io::Result<Duration> {
    let fail = || invalid("invalid calendar duration");
    let value = value.strip_prefix('+').unwrap_or(value).strip_prefix('P').ok_or_else(fail)?;
    let mut time = false;
    let mut number = String::new();
    let mut seconds = 0i64;
    let mut last = i64::MAX;
    for ch in value.chars() {
        if ch.is_ascii_digit() {
            number.push(ch);
            continue;
        }
        if ch == 'T' && !time && number.is_empty() {
            time = true;
            continue;
        }
        let unit = match (time, ch) {
            (false, 'W') if seconds == 0 => 604800,
            (false, 'D') if last != 604800 => 86400,
            (true, 'H') if last != 604800 => 3600,
            (true, 'M') => 60,
            (true, 'S') => 1,
            _ => return Err(fail()),
        };
        if unit >= last {
            return Err(fail());
        }
        last = unit;
        seconds = number.parse::<i64>().ok().and_then(|n| n.checked_mul(unit)).and_then(|n| n.checked_add(seconds)).ok_or_else(fail)?;
        number.clear();
    }
    if !number.is_empty() || last == i64::MAX || value.ends_with('T') {
        return Err(fail());
    }
    Duration::try_seconds(seconds).ok_or_else(fail)
}

fn event(properties: &[(String, String, String)]) -> io::Result<CalendarEventIn> {
    let field = |key: &str| properties.iter().find(|(name, _, _)| name == key).map(|(_, par, val)| (par.as_str(), val.as_str()));
    let value = |key| field(key).map(|(_, v)| v).unwrap_or_default();
    let uid = value("UID").to_owned();
    if uid.is_empty() {
        return Err(invalid("calendar event has no UID"));
    }
    let (start_params, start_value) = field("DTSTART").ok_or_else(|| invalid("calendar event has no DTSTART"))?;
    let (starts_at, timezone, all_day) = ical_datetime(start_value, start_params, "UTC")?;
    let ends_at = if let Some((par, val)) = field("DTEND") {
        ical_datetime(val, par, &timezone)?.0
    } else {
        let duration = field("DURATION").map(|(_, value)| ical_duration(value)).transpose()?.unwrap_or(Duration::days(i64::from(all_day)));
        starts_at.checked_add_signed(duration).ok_or_else(|| invalid("calendar end date out of range"))?
    };
    let status = field("STATUS").map_or_else(|| "confirmed".into(), |(_, s)| s.to_ascii_lowercase());
    let organizer = value("ORGANIZER");
    Ok(CalendarEventIn {
        uid,
        recurrence_id: value("RECURRENCE-ID").into(),
        title: text(value("SUMMARY")),
        description: text(value("DESCRIPTION")),
        location: text(value("LOCATION")),
        organizer: organizer.strip_prefix("mailto:").unwrap_or(organizer).into(),
        attendees: properties
            .iter()
            .filter(|(name, _, _)| name == "ATTENDEE")
            .map(|(_, _, value)| value.strip_prefix("mailto:").unwrap_or(value).into())
            .collect(),
        starts_at,
        ends_at,
        timezone,
        all_day,
        status,
        rrule: field("RRULE").map(|(_, s)| s.into()),
        etag: None,
    })
}

pub fn parse_ics(input: &str) -> io::Result<Vec<CalendarEventIn>> {
    if input.len() > MAX_BYTES {
        return Err(invalid("calendar snapshot exceeds size limit"));
    }
    let mut lines = input.trim_start_matches('\u{feff}').lines().peekable();
    let mut stack = Vec::new();
    let mut properties = Vec::new();
    let mut events = Vec::new();
    let mut complete = false;
    while let Some(line) = lines.next() {
        if line.starts_with([' ', '\t']) {
            return Err(invalid("unexpected calendar continuation"));
        }
        let mut line = Cow::Borrowed(line);
        while let Some(continuation) = lines.peek().and_then(|l| l.strip_prefix([' ', '\t'])) {
            line.to_mut().push_str(continuation);
            lines.next();
        }
        if line.is_empty() {
            continue;
        }
        let at = offset(&line, ':').ok_or_else(|| invalid("invalid calendar property"))?;
        let (key, value) = (&line[..at], &line[at + 1..]);
        let (key, params) = key.split_once(';').unwrap_or((key, ""));
        let key = key.to_ascii_uppercase();
        match key.as_str() {
            "BEGIN" => {
                let component = value.to_ascii_uppercase();
                if stack.is_empty() && component != "VCALENDAR" {
                    return Err(invalid("expected VCALENDAR wrapper"));
                }
                if stack.len() >= 32 {
                    return Err(invalid("calendar component nesting limit exceeded"));
                }
                stack.push(component);
            }
            "END" => {
                let component =
                    stack.pop().filter(|c| c.eq_ignore_ascii_case(value)).ok_or_else(|| invalid("unbalanced calendar component"))?;
                if component == "VEVENT" && stack.as_slice() == ["VCALENDAR"] {
                    if events.len() == MAX_EVENTS {
                        return Err(invalid("calendar snapshot exceeds event limit"));
                    }
                    events.push(event(&properties)?);
                    properties.clear();
                }
                if stack.is_empty() {
                    complete = true;
                }
            }
            _ if stack.as_slice() == ["VCALENDAR", "VEVENT"] => properties.push((key, params.into(), value.into())),
            _ if stack.is_empty() => return Err(invalid("calendar property outside VCALENDAR")),
            _ => {}
        }
    }
    if !complete || !stack.is_empty() {
        return Err(invalid("incomplete calendar snapshot"));
    }
    Ok(events)
}

pub enum Snapshot {
    Unchanged,
    Complete { events: Vec<CalendarEventIn>, etag: Option<String> },
}

pub enum Auth<'a> {
    Basic(&'a str, &'a str),
    Bearer(&'a str),
}

async fn send(request: RequestBuilder) -> io::Result<Response> {
    // reqwest errors can include the resolved URL; never propagate them.
    request.send().await.map_err(|_| invalid("calendar request failed"))
}

async fn response_text(mut response: Response) -> io::Result<String> {
    if !response.status().is_success() {
        return Err(invalid("calendar source returned a non-success status"));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| invalid("calendar response was interrupted"))? {
        if body.len() + chunk.len() > MAX_BYTES {
            return Err(invalid("calendar response exceeds size limit"));
        }
        body.extend_from_slice(&chunk);
    }
    String::from_utf8(body).map_err(|_| invalid("calendar response is not UTF-8"))
}

pub async fn fetch_ics_source(client: &Client, url: &str, auth: Option<Auth<'_>>, etag: Option<&str>) -> io::Result<Snapshot> {
    let url = Url::parse(url).map_err(|_| invalid("invalid calendar source URL"))?;
    let (body, etag) = match url.scheme() {
        "file" => {
            let path = url.to_file_path().map_err(|_| invalid("invalid calendar file URL"))?;
            let mut text = String::new();
            tokio::fs::File::open(path)
                .await
                .map_err(|_| invalid("calendar file unavailable"))?
                .take((MAX_BYTES + 1) as u64)
                .read_to_string(&mut text)
                .await
                .map_err(|_| invalid("calendar file unreadable"))?;
            (text, None)
        }
        "http" | "https" => {
            let mut request = client.get(url);
            request = match auth {
                Some(Auth::Basic(user, pass)) => request.basic_auth(user, Some(pass)),
                Some(Auth::Bearer(token)) => request.bearer_auth(token),
                None => request,
            };
            if let Some(etag) = etag {
                request = request.header(reqwest::header::IF_NONE_MATCH, etag);
            }
            let response = send(request).await?;
            if response.status() == StatusCode::NOT_MODIFIED {
                if etag.is_none() {
                    return Err(invalid("unexpected calendar not-modified response"));
                }
                return Ok(Snapshot::Unchanged);
            }
            if response.status() != StatusCode::OK {
                return Err(invalid("calendar source did not return a complete snapshot"));
            }
            let etag = response.headers().get(reqwest::header::ETAG).and_then(|v| v.to_str().ok()).map(str::to_owned);
            (response_text(response).await?, etag)
        }
        _ => return Err(invalid("unsupported calendar source scheme")),
    };
    Ok(Snapshot::Complete { events: parse_ics(&body)?, etag })
}

pub async fn fetch_caldav_source(client: &Client, url: &str, username: &str, password: &str) -> io::Result<Snapshot> {
    const REPORT: &str = r#"<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><d:getetag/><c:calendar-data/></d:prop><c:filter><c:comp-filter name="VCALENDAR"><c:comp-filter name="VEVENT"/></c:comp-filter></c:filter></c:calendar-query>"#;
    let response = send(
        client
            .request(reqwest::Method::from_bytes(b"REPORT").expect("valid method"), url)
            .basic_auth(username, Some(password))
            .header("Depth", "1")
            .header(reqwest::header::CONTENT_TYPE, "application/xml")
            .body(REPORT),
    )
    .await?;
    if response.status() != StatusCode::MULTI_STATUS {
        return Err(invalid("CalDAV REPORT did not return multistatus"));
    }
    let body = response_text(response).await?;
    let options = roxmltree::ParsingOptions { nodes_limit: 250_000, ..Default::default() };
    let document = roxmltree::Document::parse_with_options(&body, options).map_err(|_| invalid("invalid CalDAV XML"))?;
    let root = document.root_element();
    if !root.has_tag_name(("DAV:", "multistatus")) {
        return Err(invalid("missing CalDAV multistatus"));
    }
    let mut events = Vec::new();
    for item in root.children().filter(|n| n.has_tag_name(("DAV:", "response"))) {
        let mut found = false;
        for propstat in item.children().filter(|n| n.has_tag_name(("DAV:", "propstat"))) {
            let status = propstat.children().find(|n| n.has_tag_name(("DAV:", "status"))).and_then(|n| n.text());
            if status.and_then(|s| s.split_ascii_whitespace().nth(1)) != Some("200") {
                continue;
            }
            let data =
                propstat.descendants().find(|n| n.has_tag_name(("urn:ietf:params:xml:ns:caldav", "calendar-data"))).and_then(|n| n.text());
            if let Some(data) = data {
                found = true;
                let etag = propstat.descendants().find(|n| n.has_tag_name(("DAV:", "getetag"))).and_then(|n| n.text());
                for mut event in parse_ics(data)? {
                    if events.len() == MAX_EVENTS {
                        return Err(invalid("calendar snapshot exceeds event limit"));
                    }
                    event.etag = etag.map(str::to_owned);
                    events.push(event);
                }
            }
        }
        if !found {
            return Err(invalid("incomplete CalDAV response"));
        }
    }
    Ok(Snapshot::Complete { events, etag: None })
}

#[derive(Deserialize)]
struct ServiceAccount {
    client_email: String,
    private_key: String,
}

fn google_assertion(credentials: &str) -> io::Result<String> {
    let fail = || invalid("invalid Google service account credentials");
    let account: ServiceAccount = serde_json::from_str(credentials).map_err(|_| fail())?;
    let pem = account.private_key.lines().filter(|line| !line.starts_with("-----")).collect::<String>();
    let der = STANDARD.decode(pem).map_err(|_| fail())?;
    let key = RsaKeyPair::from_pkcs8(&der).map_err(|_| fail())?;
    let now = Utc::now().timestamp();
    let claims = json!({
        "iss": account.client_email, "scope": "https://www.googleapis.com/auth/calendar.readonly",
        "aud": "https://oauth2.googleapis.com/token", "iat": now, "exp": now + 3600
    });
    let signing_input =
        format!("{}.{}", URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#), URL_SAFE_NO_PAD.encode(claims.to_string()));
    let mut signature = vec![0; key.public().modulus_len()];
    key.sign(&RSA_PKCS1_SHA256, &SystemRandom::new(), signing_input.as_bytes(), &mut signature).map_err(|_| fail())?;
    Ok(format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(signature)))
}

fn google_date(value: &Value) -> io::Result<(DateTime<Utc>, bool)> {
    if let Some(time) = value["dateTime"].as_str() {
        return DateTime::parse_from_rfc3339(time)
            .map(|date| (date.with_timezone(&Utc), false))
            .map_err(|_| invalid("invalid Google event time"));
    }
    let day = value["date"].as_str().ok_or_else(|| invalid("missing Google event date"))?;
    let date = NaiveDate::parse_from_str(day, "%Y-%m-%d").map_err(|_| invalid("invalid Google event date"))?;
    Ok((date.and_hms_opt(0, 0, 0).expect("valid midnight").and_utc(), true))
}

pub async fn fetch_google_calendar_api(client: &Client, credentials: &str, calendar_id: &str) -> io::Result<Snapshot> {
    let assertion = google_assertion(credentials)?;
    let response = send(
        client
            .post("https://oauth2.googleapis.com/token")
            .form(&[("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"), ("assertion", assertion.as_str())]),
    )
    .await?;
    if response.status() != StatusCode::OK {
        return Err(invalid("Google OAuth request failed"));
    }
    let auth: Value = serde_json::from_str(&response_text(response).await?).map_err(|_| invalid("invalid Google OAuth response"))?;
    let token = auth["access_token"].as_str().ok_or_else(|| invalid("missing Google access token"))?;
    let mut url = Url::parse("https://www.googleapis.com/calendar/v3/calendars/").expect("valid Google URL");
    url.path_segments_mut().expect("hierarchical URL").pop_if_empty().push(calendar_id).push("events");
    let mut page = None;
    let mut events = Vec::new();
    for _ in 0..100 {
        let mut request = client.get(url.clone()).bearer_auth(token).query(&[("maxResults", "250")]);
        if let Some(value) = page.as_deref() {
            request = request.query(&[("pageToken", value)]);
        }
        let response = send(request).await?;
        if response.status() != StatusCode::OK {
            return Err(invalid("Google calendar did not return a complete page"));
        }
        let data: Value = serde_json::from_str(&response_text(response).await?).map_err(|_| invalid("invalid Google calendar response"))?;
        for item in data["items"].as_array().ok_or_else(|| invalid("incomplete Google calendar response"))? {
            let field = |key: &str| item[key].as_str().unwrap_or_default();
            if field("status") == "cancelled" && item["start"].is_null() {
                continue;
            }
            if events.len() == MAX_EVENTS {
                return Err(invalid("calendar snapshot exceeds event limit"));
            }
            let (starts_at, all_day) = google_date(&item["start"])?;
            let (ends_at, _) = google_date(&item["end"])?;
            let uid = item["iCalUID"].as_str().filter(|v| !v.is_empty()).ok_or_else(|| invalid("Google event has no iCalUID"))?;
            let recurrence_id = item["originalStartTime"]["dateTime"].as_str().or_else(|| item["originalStartTime"]["date"].as_str());
            events.push(CalendarEventIn {
                uid: uid.into(),
                recurrence_id: recurrence_id.unwrap_or_default().into(),
                title: field("summary").into(),
                description: field("description").into(),
                location: field("location").into(),
                organizer: item["organizer"]["email"].as_str().unwrap_or_default().into(),
                attendees: item["attendees"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|a| a["email"].as_str().map(str::to_owned))
                    .collect(),
                starts_at,
                ends_at,
                timezone: item["start"]["timeZone"].as_str().or_else(|| data["timeZone"].as_str()).unwrap_or("UTC").into(),
                all_day,
                status: item["status"].as_str().unwrap_or("confirmed").into(),
                rrule: item["recurrence"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .find_map(|s| s.strip_prefix("RRULE:"))
                    .map(str::to_owned),
                etag: item["etag"].as_str().map(str::to_owned),
            });
        }
        page = data["nextPageToken"].as_str().map(str::to_owned);
        if page.is_none() {
            return Ok(Snapshot::Complete { events, etag: None });
        }
    }
    Err(invalid("Google calendar snapshot exceeds page limit"))
}
