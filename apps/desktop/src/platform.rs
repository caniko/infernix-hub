//! Editor model for a release's opaque `platform_settings`
//! (`FreezeReleaseIn.platform_settings`, frozen verbatim into every
//! `ReleaseAction`).
//!
//! Only the keys the selected accounts' channels actually read are editable:
//! the `plinth` pack channel (blog metadata) and the `n8n:<platform>` workflows
//! documented in `studio-pipeline/n8n/README.md`. A channel with no known key
//! set gets no editor at all, because a generic `visibility`/`ai_disclosure`
//! that the channel ignores is worse than no control: choosing "private" would
//! still publish publicly, with the choice inside the approval hash.
//!
//! Unknown keys in the frozen value are preserved. Empty text, empty lists and
//! a flag equal to its channel default are omitted, so an untouched editor
//! produces `{}`.

use std::collections::BTreeMap;

use contracts::Account;
use serde_json::{Map, Value, json};

/// Widget for one channel key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text,
    LongText,
    /// Comma-separated list, written as a JSON array of strings.
    Tags,
    /// Boolean; `default` is what the channel does when the key is absent.
    Flag {
        default: bool,
    },
    /// One of the channel's documented values.
    Choice(&'static [&'static str]),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field {
    pub key: &'static str,
    pub label: &'static str,
    pub kind: Kind,
}

/// The `platform_settings` contract of one channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    /// Stable id: `plinth` or `n8n:<platform>`.
    pub id: &'static str,
    pub label: &'static str,
    pub fields: &'static [Field],
}

const PRIVACY: &[&str] = &["private", "unlisted", "public"];
const MASTODON_VISIBILITY: &[&str] = &["private", "unlisted", "public", "direct"];

/// Plinth `PublishArticleRequest` metadata (canix-hub `plinth` channel).
const PLINTH: Profile = Profile {
    id: "plinth",
    label: "Plinth blog",
    fields: &[
        Field { key: "title", label: "Title", kind: Kind::Text },
        Field { key: "slug", label: "Slug", kind: Kind::Text },
        Field { key: "description", label: "Description", kind: Kind::LongText },
        Field { key: "tags", label: "Tags", kind: Kind::Tags },
        Field { key: "featured", label: "Featured", kind: Kind::Flag { default: false } },
        Field { key: "published", label: "Published", kind: Kind::Flag { default: false } },
    ],
};

/// n8n contract v1, per-platform keys exactly as the README documents them.
const YOUTUBE: Profile = Profile {
    id: "n8n:youtube",
    label: "YouTube",
    fields: &[
        Field { key: "title", label: "Video title", kind: Kind::Text },
        Field { key: "description", label: "Description", kind: Kind::LongText },
        Field { key: "tags", label: "Tags", kind: Kind::Tags },
        Field { key: "category_id", label: "Category id", kind: Kind::Text },
        Field { key: "privacy_status", label: "Privacy", kind: Kind::Choice(PRIVACY) },
        Field { key: "publish_at", label: "Publish at (RFC 3339)", kind: Kind::Text },
        Field { key: "made_for_kids", label: "Made for kids", kind: Kind::Flag { default: false } },
        Field { key: "paid_promotion", label: "Paid promotion", kind: Kind::Flag { default: false } },
    ],
};

const BLUESKY: Profile = Profile {
    id: "n8n:bluesky",
    label: "Bluesky",
    fields: &[
        Field { key: "langs", label: "Languages", kind: Kind::Tags },
        Field { key: "paid_promotion", label: "Paid promotion", kind: Kind::Flag { default: false } },
    ],
};

const MASTODON: Profile = Profile {
    id: "n8n:mastodon",
    label: "Mastodon",
    fields: &[
        Field { key: "visibility", label: "Visibility", kind: Kind::Choice(MASTODON_VISIBILITY) },
        Field { key: "sensitive", label: "Sensitive media", kind: Kind::Flag { default: false } },
        Field { key: "spoiler_text", label: "Content warning", kind: Kind::Text },
        Field { key: "language", label: "Language", kind: Kind::Text },
        Field { key: "paid_promotion", label: "Paid promotion", kind: Kind::Flag { default: false } },
    ],
};

const THREADS: Profile = Profile {
    id: "n8n:threads",
    label: "Threads",
    fields: &[
        Field { key: "is_made_with_ai", label: "Made with AI", kind: Kind::Flag { default: true } },
        Field { key: "paid_partnership", label: "Paid partnership", kind: Kind::Flag { default: false } },
    ],
};

const FACEBOOK_PAGE: Profile = Profile {
    id: "n8n:facebook-page",
    label: "Facebook Page",
    fields: &[
        Field { key: "is_ai_generated", label: "AI generated", kind: Kind::Flag { default: true } },
        Field { key: "paid_promotion", label: "Paid promotion", kind: Kind::Flag { default: false } },
    ],
};

const LINKEDIN: Profile = Profile {
    id: "n8n:linkedin",
    label: "LinkedIn",
    fields: &[Field { key: "paid_promotion", label: "Paid promotion", kind: Kind::Flag { default: false } }],
};

const PROFILES: &[&Profile] = &[&PLINTH, &YOUTUBE, &BLUESKY, &MASTODON, &THREADS, &FACEBOOK_PAGE, &LINKEDIN];

/// The channel behind one account, when its key contract is known.
///
/// A `fixture`/`postiz` transport has no per-channel contract and
/// `n8n:instagram` is not in v1, so both return `None`: no editor, no keys
/// that nothing reads.
pub fn profile_for(account: &Account) -> Option<&'static Profile> {
    if let Some(platform) = account.transport.strip_prefix("n8n:") {
        return n8n_profile(platform);
    }
    (account.transport == "plinth").then_some(&PLINTH)
}

fn n8n_profile(platform: &str) -> Option<&'static Profile> {
    PROFILES.iter().copied().find(|profile| profile.id.strip_prefix("n8n:") == Some(platform))
}

/// Profiles of the selected accounts, de-duplicated in selection order.
pub fn profiles_for(accounts: &[Account], selected: &[String]) -> Vec<&'static Profile> {
    let mut profiles: Vec<&'static Profile> = Vec::new();
    for account in accounts.iter().filter(|account| selected.contains(&account.id)) {
        if let Some(profile) = profile_for(account) {
            if !profiles.iter().any(|known| known.id == profile.id) {
                profiles.push(profile);
            }
        }
    }
    profiles
}

/// Editable values for one profile.
#[derive(Debug, Clone, PartialEq)]
pub struct Draft {
    profile: &'static Profile,
    text: BTreeMap<&'static str, String>,
    flags: BTreeMap<&'static str, bool>,
}

impl Draft {
    /// Read every field; an absent key carries the channel's default.
    pub fn read(profile: &'static Profile, value: &Value) -> Self {
        let object = value.as_object();
        let mut text = BTreeMap::new();
        let mut flags = BTreeMap::new();
        for field in profile.fields {
            let entry = object.and_then(|object| object.get(field.key));
            match field.kind {
                Kind::Flag { default } => {
                    flags.insert(field.key, entry.and_then(Value::as_bool).unwrap_or(default));
                }
                Kind::Tags => {
                    let tags = entry
                        .and_then(Value::as_array)
                        .map(|items| items.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "))
                        .unwrap_or_default();
                    text.insert(field.key, tags);
                }
                Kind::Text | Kind::LongText | Kind::Choice(_) => {
                    text.insert(field.key, entry.and_then(Value::as_str).unwrap_or_default().to_string());
                }
            }
        }
        Self { profile, text, flags }
    }

    pub fn profile(&self) -> &'static Profile {
        self.profile
    }

    pub fn text(&self, key: &str) -> &str {
        self.text.get(key).map(String::as_str).unwrap_or_default()
    }

    pub fn flag(&self, key: &str) -> bool {
        self.flags.get(key).copied().unwrap_or(false)
    }

    /// What this editor writes for every field: `Some` is a value, `None` is a
    /// removal (an empty text, an empty list, or a flag left at its default).
    /// `apply_to` and conflict detection both read this one map, so they can
    /// never disagree about what an editor writes.
    fn writes(&self) -> BTreeMap<&'static str, Option<Value>> {
        let mut writes = BTreeMap::new();
        for field in self.profile.fields {
            let write = match field.kind {
                // Writing the default would only add a key the channel already
                // implies; a non-default choice must be explicit.
                Kind::Flag { default } => (self.flag(field.key) != default).then(|| json!(self.flag(field.key))),
                Kind::Tags => {
                    let tags = split_list(self.text(field.key));
                    (!tags.is_empty()).then(|| json!(tags))
                }
                Kind::Text | Kind::LongText | Kind::Choice(_) => {
                    let text = self.text(field.key).trim();
                    (!text.is_empty()).then(|| json!(text))
                }
            };
            writes.insert(field.key, write);
        }
        writes
    }

    fn apply_to(&self, object: &mut Map<String, Value>) {
        for (key, write) in self.writes() {
            match write {
                Some(value) => {
                    object.insert(key.into(), value);
                }
                None => {
                    object.remove(key);
                }
            }
        }
    }
}

/// Two editors write one shared `platform_settings` key differently. The
/// frozen value is one flat object, so that key's value would depend on
/// profile order; freezing must not silently pick one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub key: &'static str,
    /// Channel labels that disagree, in selection order.
    pub channels: Vec<&'static str>,
}

impl Conflict {
    pub fn message(&self) -> String {
        format!(
            "\"{}\" is set differently by {}. Set the same value in each editor; leaving it untouched in one clears it for all.",
            self.key,
            self.channels.join(", ")
        )
    }
}

/// Drafts for every channel the selected accounts publish through.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SettingsDraft {
    drafts: Vec<Draft>,
}

impl SettingsDraft {
    /// Keep the drafts for `profiles`, reading new ones from `value`. A draft
    /// already present keeps the operator's edits, so selecting a second
    /// account does not reset the first.
    pub fn sync(&mut self, value: &Value, profiles: &[&'static Profile]) {
        self.drafts = profiles
            .iter()
            .map(|profile| {
                self.drafts.iter().find(|draft| draft.profile.id == profile.id).cloned().unwrap_or_else(|| Draft::read(profile, value))
            })
            .collect();
    }

    pub fn drafts(&self) -> &[Draft] {
        &self.drafts
    }

    pub fn set_text(&mut self, profile: &'static str, key: &'static str, value: impl Into<String>) {
        if let Some(draft) = self.drafts.iter_mut().find(|draft| draft.profile.id == profile) {
            draft.text.insert(key, value.into());
        }
    }

    pub fn set_flag(&mut self, profile: &'static str, key: &'static str, value: bool) {
        if let Some(draft) = self.drafts.iter_mut().find(|draft| draft.profile.id == profile) {
            draft.flags.insert(key, value);
        }
    }

    /// Shared keys that two selected editors would write differently. Callers
    /// must block a freeze on any result: applying the drafts in order would
    /// silently keep one channel's value for every channel.
    pub fn conflicts(&self) -> Vec<Conflict> {
        let mut by_key: BTreeMap<&'static str, Vec<(&'static str, Option<Value>)>> = BTreeMap::new();
        for draft in &self.drafts {
            for (key, write) in draft.writes() {
                by_key.entry(key).or_default().push((draft.profile.label, write));
            }
        }
        by_key
            .into_iter()
            .filter_map(|(key, writes)| {
                let first = &writes[0].1;
                writes
                    .iter()
                    .any(|(_, write)| write != first)
                    .then(|| Conflict { key, channels: writes.into_iter().map(|(channel, _)| channel).collect() })
            })
            .collect()
    }

    /// Apply every draft on top of the frozen value, preserving unknown keys.
    pub fn apply(&self, base: &Value) -> Value {
        let mut object = base.as_object().cloned().unwrap_or_default();
        for draft in &self.drafts {
            draft.apply_to(&mut object);
        }
        // A slug makes the action a Plinth article: `published` is always
        // written so the channel never guesses it from frontmatter.
        if object.get("slug").and_then(Value::as_str).is_some_and(|slug| !slug.trim().is_empty()) {
            let published = object.get("published").and_then(Value::as_bool).unwrap_or(false);
            object.insert("published".into(), json!(published));
        }
        Value::Object(object)
    }
}

/// Split a comma-separated list, trimming and dropping empties.
fn split_list(value: &str) -> Vec<String> {
    value.split(',').map(str::trim).filter(|item| !item.is_empty()).map(str::to_string).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(id: &str, transport: &str) -> Account {
        Account {
            id: id.into(),
            company_id: "fixture-company".into(),
            platform: "mastodon".into(),
            handle: "@juniper@example.social".into(),
            transport: transport.into(),
            persona_id: None,
            capabilities: contracts::AccountCapabilities::default(),
        }
    }

    fn draft(profile: &'static Profile, value: &Value) -> SettingsDraft {
        let mut draft = SettingsDraft::default();
        draft.sync(value, &[profile]);
        draft
    }

    #[test]
    fn blog_fields_round_trip_and_published_is_always_frozen() {
        let value = json!({
            "title": "Field notes",
            "slug": "field-notes",
            "description": "a post",
            "tags": ["ai", "video"],
            "featured": true,
            "published": false,
        });
        let draft = draft(&PLINTH, &value);
        assert_eq!(draft.drafts()[0].profile().id, "plinth");
        assert_eq!(draft.drafts()[0].text("title"), "Field notes");
        assert!(draft.drafts()[0].flag("featured"));
        assert!(!draft.drafts()[0].flag("published"));
        let canonical = draft.apply(&value);
        assert_eq!(draft.apply(&canonical), canonical, "apply is idempotent");
        assert_eq!(canonical["published"], json!(false), "an article always freezes published");
        assert_eq!(canonical["tags"], json!(["ai", "video"]));
    }

    #[test]
    fn youtube_privacy_replaces_the_generic_visibility_key() {
        let base = json!({ "visibility": "private", "mystery": [1, 2] });
        let mut draft = draft(&YOUTUBE, &base);
        assert_eq!(draft.drafts()[0].text("privacy_status"), "", "absent privacy_status means the channel default (private)");
        draft.set_text("n8n:youtube", "privacy_status", "unlisted");
        draft.set_flag("n8n:youtube", "made_for_kids", true);
        let value = draft.apply(&base);
        assert_eq!(value["privacy_status"], "unlisted", "the channel's own key is written");
        assert_eq!(value["made_for_kids"], true);
        assert_eq!(value["visibility"], "private", "unknown frozen keys survive an edit");
        assert_eq!(value["mystery"], json!([1, 2]));
        assert!(value.get("ai_disclosure").is_none(), "no generic disclosure key is ever written");
    }

    #[test]
    fn a_flags_default_is_absent_until_the_operator_changes_it() {
        // Threads treats an absent is_made_with_ai as true.
        let draft = draft(&THREADS, &json!({}));
        assert!(draft.drafts()[0].flag("is_made_with_ai"), "the channel default is shown");
        assert_eq!(draft.apply(&json!({})), json!({}), "an untouched default writes nothing");

        let mut disabled = draft.clone();
        disabled.set_flag("n8n:threads", "is_made_with_ai", false);
        assert_eq!(disabled.apply(&json!({})), json!({"is_made_with_ai": false}), "a non-default choice is explicit");
    }

    #[test]
    fn unknown_channels_get_no_editor() {
        assert!(profile_for(&account("a", "fixture")).is_none());
        assert!(profile_for(&account("a", "postiz")).is_none());
        assert!(profile_for(&account("a", "n8n:instagram")).is_none());
        assert_eq!(profile_for(&account("a", "plinth")).map(|p| p.id), Some("plinth"));
        assert_eq!(profile_for(&account("a", "n8n:youtube")).map(|p| p.id), Some("n8n:youtube"));
    }

    #[test]
    fn selected_accounts_dedupe_profiles_and_skip_unknown_channels() {
        let accounts =
            [account("yt-1", "n8n:youtube"), account("yt-2", "n8n:youtube"), account("blog", "plinth"), account("fixture", "fixture")];
        let selected = vec!["yt-1".to_string(), "yt-2".to_string(), "blog".to_string(), "fixture".to_string()];
        let profiles = profiles_for(&accounts, &selected);
        assert_eq!(profiles.iter().map(|p| p.id).collect::<Vec<_>>(), vec!["n8n:youtube", "plinth"]);
        assert!(profiles_for(&accounts, &["fixture".to_string()]).is_empty(), "a simulated transport has no keys to edit");
    }

    #[test]
    fn clearing_a_key_removes_it_and_unknown_keys_survive() {
        let base = json!({"title": "old", "slug": "old-slug", "youtube": {"privacy_status": "unlisted"}});
        let mut draft = draft(&PLINTH, &base);
        draft.set_text("plinth", "title", "");
        let value = draft.apply(&base);
        assert!(value.get("title").is_none(), "a cleared key is removed");
        assert_eq!(value["youtube"]["privacy_status"], "unlisted", "per-platform objects survive an edit");
        assert_eq!(value["slug"], "old-slug", "an unedited slug is preserved");
        assert_eq!(value["published"], json!(false), "a slug-bearing article always freezes published");
    }

    #[test]
    fn tags_split_on_commas_and_untouched_editors_write_nothing() {
        let mut draft = draft(&BLUESKY, &json!({ "langs": ["en", "de"] }));
        assert_eq!(draft.drafts()[0].text("langs"), "en, de");
        draft.set_text("n8n:bluesky", "langs", " en , ,fr ");
        assert_eq!(draft.apply(&json!({})), json!({"langs": ["en", "fr"]}));
        assert_eq!(SettingsDraft::default().apply(&json!({})), json!({}));
        assert_eq!(SettingsDraft::default().apply(&json!(null)), json!({}));
    }

    #[test]
    fn a_shared_key_written_differently_blocks_the_freeze_and_names_both_channels() {
        let mut draft = SettingsDraft::default();
        draft.sync(&json!({}), &[&PLINTH, &YOUTUBE]);
        // The same value in both editors is one shared key, not a conflict.
        draft.set_text("plinth", "title", "Field notes");
        draft.set_text("n8n:youtube", "title", "Field notes");
        assert!(draft.conflicts().is_empty(), "{:?}", draft.conflicts());

        draft.set_text("n8n:youtube", "title", "A video title");
        let conflicts = draft.conflicts();
        assert_eq!(conflicts.len(), 1, "{conflicts:?}");
        assert_eq!(conflicts[0].key, "title");
        assert_eq!(conflicts[0].channels, vec!["Plinth blog", "YouTube"]);
        let message = conflicts[0].message();
        for name in ["title", "Plinth blog", "YouTube"] {
            assert!(message.contains(name), "the error names {name}: {message}");
        }
    }

    #[test]
    fn a_key_set_in_one_editor_and_left_untouched_in_another_is_a_conflict() {
        let mut draft = SettingsDraft::default();
        draft.sync(&json!({}), &[&YOUTUBE, &BLUESKY]);
        draft.set_flag("n8n:youtube", "paid_promotion", true);
        let conflicts = draft.conflicts();
        assert_eq!(conflicts.len(), 1, "a later editor's default would silently clear the flag: {conflicts:?}");
        assert_eq!(conflicts[0].key, "paid_promotion");
        // Setting the same value everywhere resolves it.
        draft.set_flag("n8n:bluesky", "paid_promotion", true);
        assert!(draft.conflicts().is_empty(), "{:?}", draft.conflicts());
    }

    #[test]
    fn channels_with_independent_keys_never_conflict() {
        let mut draft = SettingsDraft::default();
        draft.sync(&json!({}), &[&BLUESKY, &MASTODON]);
        draft.set_text("n8n:bluesky", "langs", "en");
        draft.set_text("n8n:mastodon", "language", "en");
        assert!(draft.conflicts().is_empty(), "{:?}", draft.conflicts());
    }
}
