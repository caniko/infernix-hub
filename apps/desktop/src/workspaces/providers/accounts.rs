use std::collections::BTreeMap;

use super::{
    dashboard::{AccountIdentity, account_name, is_next_pick, sorted_accounts},
    *,
};

#[component]
pub(super) fn Accounts(mut providers: Vec<ProviderSummary>) -> Element {
    providers.sort_by_key(|provider| {
        use ProviderCapability::*;
        ![
            BeginOAuth,
            ImportLogin,
            RenewLogin,
            RemoveLogin,
            ExportLogin,
            SelectLocalAccount,
            SelectPersistentAccount,
            SetEnabled,
            SetPriority,
            SetWeight,
            SetNote,
            ManageKeyGroups,
            ManageKeys,
            ToggleKey,
            SetGroupEnabled,
            SetRequestIdentity,
            ConfigureModels,
            DiscoverModels,
            SetModelAliases,
        ]
        .into_iter()
        .any(|cap| supports(provider, None, cap))
    });
    rsx! { for provider in providers {
        AccountProvider { key: "{provider.descriptor.id}", provider }
    } }
}

#[component]
fn AccountProvider(provider: ProviderSummary) -> Element {
    let prefs = use_context::<PanelState>().settings.cloned();
    let mut hint = use_signal(String::new);
    let mut login_label = use_signal(String::new);
    let mut import_label = use_signal(String::new);
    let operator = use_context::<PanelState>().operator();
    let can_import = operator && supports(&provider, None, ProviderCapability::ImportLogin);
    let can_connect = operator && supports(&provider, None, ProviderCapability::BeginOAuth);
    rsx! {
        section { class: "glass-card stack",
            div { class: "row",
                h2 { "{provider.descriptor.label} · Logins" }
                div { class: "spacer" }
                OperationButton { provider: provider.clone(), label: "Renew all logins", confirm: true,
                    command: ProviderCommand::Credential(CredentialCommand::RenewLogin { account_id: None }) }
            }
            if provider.descriptor.capabilities.is_empty() { p { class: "form-help", "Read-only provider inventory." } }
            if can_connect {
                div { class: "row", role: "group", aria_label: "Add an account",
                    Field { label: "New login label (optional)", value: login_label(), change: move |v| login_label.set(v) }
                    OperationButton { provider: provider.clone(), label: "Connect account", tone: "primary",
                        command: ProviderCommand::Credential(CredentialCommand::BeginOAuth {
                            account_label: optional(&login_label()), host: None, unix_user: None,
                        }) }
                }
            }
            if can_import {
                details { summary { "Import an existing login" }
                    p { class: "form-help", "Use the source reference understood by the provider’s credential owner. Do not paste tokens or credential JSON." }
                    div { class: "row",
                        Field { label: "Login source reference", value: hint(), change: move |v| hint.set(v) }
                        Field { label: "Imported login label (optional)", value: import_label(), change: move |v| import_label.set(v) }
                        OperationButton { provider: provider.clone(), label: "Import login", confirm: true, disabled: optional(&hint()).is_none(),
                            command: ProviderCommand::Credential(CredentialCommand::ImportLogin { label: optional(&import_label()), source_hint: hint().trim().into() }) }
                    }
                }
            }
            if provider.accounts.is_empty() { p { class: "muted", "No logins or account bindings observed." } }
            div { class: "hairline-list",
                for account in sorted_accounts(&provider, prefs.account_sort, prefs.mask_emails) {
                    AccountEditor { key: "{account.id}-{account.priority}-{account.weight}", provider: provider.clone(), account: account.clone(), mask: prefs.mask_emails }
                }
            }
            KeyGroups { provider }
        }
    }
}

#[component]
fn AccountEditor(provider: ProviderSummary, account: ModelAccount, mask: bool) -> Element {
    let id = account.id.clone();
    let mut note = use_signal(|| account.note.clone().unwrap_or_default());
    let mut priority = use_signal(|| account.priority.to_string());
    let mut weight = use_signal(|| account.weight.to_string());
    let mut scope = use_signal(|| "host".to_string());
    let mut target = use_signal(|| account.host.clone().unwrap_or_default());
    let toggle = if account.enabled { "Disable" } else { "Enable" };
    let name = account_name(&account, mask);
    let operator = use_context::<PanelState>().operator();
    let can_edit = |cap| operator && supports(&provider, Some(&account), cap);
    let can_switch = !account.is_active && can_edit(ProviderCapability::SelectLocalAccount);
    let can_renew = can_edit(ProviderCapability::RenewLogin);
    let edit_persistent = can_edit(ProviderCapability::SelectPersistentAccount);
    let edit_priority = can_edit(ProviderCapability::SetPriority);
    let edit_weight = can_edit(ProviderCapability::SetWeight);
    let edit_note = can_edit(ProviderCapability::SetNote);
    rsx! {
        article { class: "hairline-row stack", style: "padding-block: 8px; gap: 4px;",
            AccountIdentity { provider: provider.clone(), account: account.clone(), mask, show_last_used: true }
            if let Some(note) = &account.note { p { "{note}" } }
            if can_switch || can_renew {
                div { class: "row",
                    if can_switch {
                        OperationButton { provider: provider.clone(), account: account.clone(),
                            label: "Switch account", disabled: !account.enabled,
                            command: ProviderCommand::Account(AccountCommand::SelectLocalAccount {
                                account_id: id.clone(), host: account.host.clone(), unix_user: account.unix_user.clone(),
                            }) }
                    }
                    if can_renew {
                        OperationButton { provider: provider.clone(), account: account.clone(), label: "Renew login", tone: "secondary",
                            command: ProviderCommand::Credential(CredentialCommand::RenewLogin { account_id: Some(id.clone()) }) }
                    }
                }
            }
            details {
                summary { aria_label: "Manage {name}", "Manage" }
                p { class: "form-help", "Routing priority: {account.priority} · Routing weight: {account.weight}" }
                if is_next_pick(&provider, &account) { p { class: "form-help", "Next pick for routing" } }
                div { class: "row",
                    OperationButton { provider: provider.clone(), account: account.clone(), label: toggle, confirm: account.enabled,
                        command: ProviderCommand::Account(AccountCommand::SetEnabled { account_id: id.clone(), enabled: !account.enabled }) }
                    OperationButton { provider: provider.clone(), account: account.clone(), label: "Export login", confirm: true,
                        command: ProviderCommand::Credential(CredentialCommand::ExportLogin { account_id: id.clone() }) }
                    OperationButton { provider: provider.clone(), account: account.clone(), label: "Remove login", tone: "danger", confirm: true,
                        command: ProviderCommand::Credential(CredentialCommand::RemoveLogin { account_id: id.clone() }) }
                }
                if edit_persistent {
                    div { class: "row",
                        Field { label: "Persistent selection scope", value: scope(), change: move |v| scope.set(v) }
                        Field { label: "Persistent target (optional)", value: target(), change: move |v| target.set(v) }
                        OperationButton { provider: provider.clone(), account: account.clone(), label: "Set persistent account", confirm: true,
                            disabled: !account.enabled || optional(&scope()).is_none(),
                            command: ProviderCommand::Account(AccountCommand::SelectPersistentAccount { account_id: id.clone(), scope: scope().trim().into(), target: optional(&target()) }) }
                    }
                }
                div { class: "row",
                    if edit_priority {
                        Field { label: "Priority", kind: "number", value: priority(), change: move |v| priority.set(v) }
                        OperationButton { provider: provider.clone(), account: account.clone(), label: "Save priority", disabled: priority().parse::<i32>().is_err(),
                            command: ProviderCommand::Account(AccountCommand::SetPriority { account_id: id.clone(), priority: priority().parse().unwrap_or_default() }) }
                    }
                    if edit_weight {
                        Field { label: "Weight", kind: "number", value: weight(), change: move |v| weight.set(v) }
                        OperationButton { provider: provider.clone(), account: account.clone(), label: "Save weight", disabled: !weight().parse::<u32>().is_ok_and(|n| n > 0),
                            command: ProviderCommand::Account(AccountCommand::SetWeight { account_id: id.clone(), weight: weight().parse().unwrap_or_default() }) }
                    }
                    if edit_note {
                        Field { label: "Note", value: note(), change: move |v| note.set(v) }
                        OperationButton { provider: provider.clone(), account: account.clone(), label: "Save note",
                            command: ProviderCommand::Account(AccountCommand::SetNote { account_id: id.clone(), note: optional(&note()) }) }
                    }
                }
            }
        }
    }
}

#[component]
fn KeyGroups(provider: ProviderSummary) -> Element {
    let client = use_context::<HubClient>();
    let panel = use_context::<PanelState>();
    let id = segment(&provider.descriptor.id);
    let cid = id.clone();
    let model_client = client.clone();
    let data = use_resource(move || {
        let (id, client) = (cid.clone(), client.clone());
        let _ = (panel.epoch)();
        async move { timed(client.provider_key_groups(&path_id(id)?)).await }
    });
    let models = use_resource(move || {
        let (id, client) = (id.clone(), model_client.clone());
        let _ = (panel.epoch)();
        async move { timed(client.provider_models(&path_id(id)?)).await }
    });
    let groups = data.read();
    let models = models.read();
    let can_manage = panel.operator() && supports(&provider, None, ProviderCapability::ManageKeyGroups);
    let can_discover = panel.operator() && supports(&provider, None, ProviderCapability::DiscoverModels);
    let model_names: BTreeMap<_, _> = models
        .as_ref()
        .and_then(|result| result.as_ref().ok())
        .into_iter()
        .flatten()
        .filter_map(|model| Some((model.id.clone(), optional(model.display_name.as_deref()?)?)))
        .collect();
    let no_groups = groups.as_ref().is_some_and(|result| result.as_ref().is_ok_and(Vec::is_empty));
    rsx! {
        div { class: "stack",
            if !no_groups || can_manage || can_discover {
                div { class: "row",
                    h3 { "API key groups" }
                    OperationButton { provider: provider.clone(), label: "Discover models",
                        command: ProviderCommand::Model(ModelCommand::DiscoverModels { group_id: None }) }
                }
            }
            {read_view(groups.as_ref(), |groups| rsx! {
                if groups.is_empty() { p { class: "form-help", "No key groups reported." } }
                for group in groups {
                    KeyGroupView { key: "{group.id}-{group.updated_at.timestamp_millis()}", provider: provider.clone(), group: group.clone(), model_names: model_names.clone() }
                }
                if can_manage {
                    details {
                        summary { "Add key group" }
                        GroupEditor { provider: provider.clone(), group: None }
                    }
                }
            })}
            if !no_groups || !models.as_ref().is_some_and(|result| result.as_ref().is_ok_and(Vec::is_empty)) {
                details { summary { "Provider model inventory" }
                    {read_view(models.as_ref(), |models| rsx! { ModelList { models: models.iter().map(|m| m.id.clone()).collect() } })}
                }
            }
        }
    }
}

#[component]
fn ModelList(models: Vec<String>) -> Element {
    if models.is_empty() {
        return rsx! { p { class: "muted", "No model IDs reported." } };
    }
    rsx! { Output { label: "Model IDs (one per line)", value: models.join("\n") } }
}

#[component]
fn KeyGroupView(provider: ProviderSummary, group: KeyGroup, model_names: BTreeMap<String, String>) -> Element {
    let mut key_label = use_signal(String::new);
    let mut key_ref = use_signal(String::new);
    let mut models = use_signal(|| group.configured_models.join("\n"));
    let mut aliases =
        use_signal(|| group.aliases.iter().map(|a| format!("{}={}", a.alias, a.target_model_id)).collect::<Vec<_>>().join("\n"));
    let id = group.id.clone();
    let enabled_cmd = KeyGroupCommand::SetGroupEnabled { group_id: id.clone(), enabled: !group.enabled };
    let mode = match group.request_identity {
        RequestIdentityMode::Api => RequestIdentityMode::AutoClient,
        RequestIdentityMode::AutoClient => RequestIdentityMode::Api,
    };
    let parsed_aliases = parse_aliases(&aliases());
    let group_action = if group.enabled { "Disable group" } else { "Enable group" };
    let identity_action = if mode == RequestIdentityMode::Api { "Use API identity" } else { "Use automatic client identity" };
    rsx! {
        article { class: "provider-inset stack",
            h3 { "{group.name}" }
            p { class: "muted", {format!("{} · {} · {} · {}", group.family, group.base_url.as_deref().unwrap_or("default endpoint"), request_identity_label(group.request_identity), if group.enabled { "Enabled" } else { "Disabled" })} }
            div { class: "row",
                OperationButton { provider: provider.clone(), label: group_action,
                    command: ProviderCommand::KeyGroup(enabled_cmd) }
                OperationButton { provider: provider.clone(), label: identity_action,
                    command: ProviderCommand::KeyGroup(KeyGroupCommand::SetRequestIdentity { group_id: id.clone(), mode }) }
                OperationButton { provider: provider.clone(), label: "Discover Models",
                    command: ProviderCommand::Model(ModelCommand::DiscoverModels { group_id: Some(id.clone()) }) }
                OperationButton { provider: provider.clone(), label: "Delete group", confirm: true,
                    command: ProviderCommand::KeyGroup(KeyGroupCommand::DeleteGroup { group_id: id.clone() }) }
            }
            h4 { "Saved keys" }
            if group.keys.is_empty() { p { class: "muted", "No saved keys." } }
            for key in &group.keys {
                {let key_action = if key.enabled { "Disable key" } else { "Enable key" }; rsx! {
                div { key: "{key.id}", class: "row",
                    span { "{key.label} · {key.key_hint} · " if key.enabled { "Enabled" } else { "Disabled" } " · {key.success_count} successful / {key.failure_count} failed" }
                    OperationButton { provider: provider.clone(), label: key_action,
                        command: ProviderCommand::KeyGroup(KeyGroupCommand::ToggleKey { group_id: id.clone(), key_id: key.id.clone(), enabled: !key.enabled }) }
                    OperationButton { provider: provider.clone(), label: "Remove key", confirm: true,
                        command: ProviderCommand::KeyGroup(KeyGroupCommand::RemoveKey { group_id: id.clone(), key_id: key.id.clone() }) }
                }
                }}
            }
            if supports(&provider, None, ProviderCapability::ManageKeys) {
                div { class: "row",
                    Field { label: "Key label", value: key_label(), change: move |v| key_label.set(v) }
                    Field { label: "Secret reference (not the key)", kind: "password", value: key_ref(), change: move |v| key_ref.set(v) }
                    OperationButton { provider: provider.clone(), label: "Add key", disabled: optional(&key_label()).is_none() || optional(&key_ref()).is_none(),
                        command: ProviderCommand::KeyGroup(KeyGroupCommand::AddKey { group_id: id.clone(), label: key_label().trim().into(), key_ref: key_ref().trim().into() }) }
                }
            }
            details { summary { "Configured models and aliases" }
                ModelList { models: group.configured_models.clone() }
                for alias in &group.aliases {
                    {let model = group.discovered_models.iter().find(|m| m.id == alias.target_model_id)
                        .and_then(|m| m.display_name.as_deref()).filter(|name| !name.trim().is_empty())
                        .or_else(|| model_names.get(&alias.target_model_id).map(String::as_str));
                    rsx! { p {
                        "{alias.alias} → "
                        if let Some(model) = model.filter(|name| *name != alias.target_model_id) {
                            "{model} " code { class: "muted", "({alias.target_model_id})" }
                        } else { code { "{alias.target_model_id}" } }
                    } }}
                }
                if supports(&provider, None, ProviderCapability::ConfigureModels) {
                    label { class: "provider-field", "Configured models, one ID per line",
                        textarea { class: "input", value: models(), oninput: move |e| models.set(e.value()) }
                    }
                    OperationButton { provider: provider.clone(), label: "Save models",
                        command: ProviderCommand::Model(ModelCommand::SetModels { group_id: id.clone(), models: lines(&models()) }) }
                }
                if supports(&provider, None, ProviderCapability::SetModelAliases) {
                    label { class: "provider-field", "Aliases, one alias=model per line",
                        textarea { class: "input", value: aliases(), oninput: move |e| aliases.set(e.value()) }
                    }
                    OperationButton { provider: provider.clone(), label: "Save aliases", disabled: parsed_aliases.is_none(),
                        command: ProviderCommand::Model(ModelCommand::SetAliases { group_id: id, aliases: parsed_aliases.unwrap_or_default() }) }
                }
            }
            details { summary { "Discovered models ({group.discovered_models.len()})" }
                ModelList { models: group.discovered_models.iter().map(|m| m.id.clone()).collect() }
            }
            if supports(&provider, None, ProviderCapability::ManageKeyGroups) {
                details { summary { "Edit key group" }
                    GroupEditor { provider, group: Some(group) }
                }
            }
        }
    }
}

fn lines(value: &str) -> Vec<String> {
    value.lines().filter_map(optional).collect()
}

fn parse_aliases(value: &str) -> Option<Vec<ModelAlias>> {
    lines(value)
        .iter()
        .map(|line| {
            let (alias, model) = line.split_once('=')?;
            Some(ModelAlias { alias: optional(alias)?, target_model_id: optional(model)? })
        })
        .collect()
}

#[component]
fn GroupEditor(provider: ProviderSummary, group: Option<KeyGroup>) -> Element {
    let mut name = use_signal(|| group.as_ref().map(|g| g.name.clone()).unwrap_or_default());
    let mut family = use_signal(|| group.as_ref().map_or_else(|| provider.descriptor.family.clone(), |g| g.family.clone()));
    let mut url = use_signal(|| group.as_ref().and_then(|g| g.base_url.clone()).unwrap_or_default());
    let base_url = optional(&url());
    let valid = !name().trim().is_empty()
        && !family().trim().is_empty()
        && base_url.as_deref().is_none_or(|s| s.starts_with("https://") || s.starts_with("http://"));
    let command = KeyGroupCommand::UpsertGroup {
        group_id: group.as_ref().map(|g| g.id.clone()),
        family: family().trim().into(),
        name: name().trim().into(),
        base_url,
        enabled: group.as_ref().is_none_or(|g| g.enabled),
        request_identity: group.as_ref().map(|g| g.request_identity).unwrap_or_default(),
        configured_models: group.as_ref().map(|g| g.configured_models.clone()).unwrap_or_default(),
        aliases: group.as_ref().map(|g| g.aliases.clone()).unwrap_or_default(),
    };
    rsx! { div { class: "row",
        Field { label: "Group name", value: name(), change: move |v| name.set(v) }
        Field { label: "Family", value: family(), change: move |v| family.set(v) }
        Field { label: "Base URL (optional)", kind: "url", value: url(), change: move |v| url.set(v) }
        OperationButton { provider, label: "Save key group", disabled: !valid, command: ProviderCommand::KeyGroup(command) }
    } }
}
