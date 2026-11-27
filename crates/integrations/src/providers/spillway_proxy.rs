use super::*;
use async_trait::async_trait;
use domain::ports::{ModelProvider, ProviderDispatch, ProviderFacets, ProviderReconciliation};
use reqwest::{Method, header::HeaderValue};
use serde_json::json;
use std::collections::BTreeMap;

/// An authenticated, per-family view of a Spillway/CLIProxyAPI management instance.
///
/// A bare URL selects Spillway's v8 API. Pass a URL ending in `/v0/management`
/// to select the reduced legacy surface. The management key is never used on `/v1`.
pub struct SpillwayProxyProvider {
    descriptor: ProviderDescriptor,
    base: Url,
    management_token_file: PathBuf,
    http: Client,
    v8: bool,
    discovered: tokio::sync::RwLock<Vec<DiscoveredModel>>,
}

impl SpillwayProxyProvider {
    pub fn new(
        id: &str,
        label: &str,
        family: &str,
        router_id: &str,
        management_url: &str,
        management_token_file: impl Into<PathBuf>,
    ) -> PortResult<Self> {
        use ProviderCapability::*;
        let mut base = base_url(management_url)?;
        let v8 = !base.path().trim_end_matches('/').ends_with("/v0/management");
        if v8 && !base.path().trim_end_matches('/').ends_with("/v8/management") {
            base = endpoint(&base, "/v8/management");
        }
        let mut descriptor = descriptor(id, label, family, AdapterMode::Live);
        descriptor.router_id = Some(router_id.into());
        descriptor.capabilities.extend([
            SetEnabled,
            SetPriority,
            SetNote,
            RemoveLogin,
            DiscoverModels,
            ReadProxyStatus,
            ReadProxyLogs,
            SetRoutePolicy,
        ]);
        if v8 {
            descriptor.capabilities.extend([SetWeight, SetCapacity, RenewLogin, SetPoolOverride, MoveConversation]);
            if matches!(family, "claude" | "codex") {
                descriptor.evidence_kinds.insert(EvidenceKind::ResetGrants);
                descriptor.capabilities.extend([RefreshQuota, RefreshGrants, ClaimResetGrant, StartRollingWindow]);
            }
            if config_family(family).is_some() {
                descriptor.capabilities.extend([ManageKeyGroups, ToggleKey, SetGroupEnabled, ConfigureModels, SetModelAliases]);
                if family == "claude" {
                    descriptor.capabilities.insert(SetRequestIdentity);
                }
            }
        } else {
            descriptor.evidence_kinds.remove(&EvidenceKind::QuotaWindows);
        }
        Ok(Self {
            descriptor,
            base,
            management_token_file: management_token_file.into(),
            http: http_client()?,
            v8,
            discovered: Default::default(),
        })
    }

    fn credential_path(&self) -> &'static str {
        if self.v8 { "/credentials" } else { "/auth-files" }
    }

    async fn token(&self) -> PortResult<String> {
        secret_file(&self.management_token_file).await.map_err(PortError::Unreachable)
    }

    fn url(&self, path: &str) -> Url {
        endpoint(&self.base, path)
    }

    async fn get(&self, token: &str, url: Url) -> PortResult<Value> {
        let response = request(self.http.get(url).bearer_auth(token), "proxy management read", false, Duration::from_secs(10)).await?;
        json_body(response, "proxy management read").await
    }

    async fn send(&self, token: &str, method: Method, url: Url, body: Option<&Value>, etag: Option<HeaderValue>) -> PortResult<Response> {
        let mut req = self.http.request(method, url).bearer_auth(token);
        if let Some(body) = body {
            req = req.json(body);
        }
        if let Some(etag) = etag {
            req = req.header(reqwest::header::IF_MATCH, etag);
        }
        request(req, "proxy management update", true, Duration::from_secs(70)).await
    }

    async fn update(&self, token: &str, method: Method, path: &str, body: Value) -> PortResult<Response> {
        self.send(token, method, self.url(path), Some(&body), None).await
    }

    fn matches_family(&self, raw: &Value) -> bool {
        str_field(&raw["provider"])
            .or_else(|| str_field(&raw["type"]))
            .is_some_and(|provider| provider.eq_ignore_ascii_case(&self.descriptor.family))
    }

    async fn inventory(&self, token: &str) -> PortResult<Value> {
        let body = self.get(token, self.url(self.credential_path())).await?;
        if !body["files"].is_array() {
            return Err(PortError::Unreachable("proxy management read: missing auth-file list".into()));
        }
        Ok(body)
    }

    async fn quota(&self, token: &str) -> PortResult<Value> {
        let body = self.get(token, self.url("/usage/windows")).await?;
        if !body["credentials"].is_array() {
            return Err(PortError::Unreachable("proxy management read: missing quota credential list".into()));
        }
        Ok(body)
    }

    async fn quota_account(&self, token: &str, hub_id: &str) -> PortResult<Value> {
        let body = self.quota(token).await?;
        rows(&body["credentials"])
            .iter()
            .find(|row| self.matches_family(row) && str_field(&row["id"]).is_some_and(|id| self.account_id(id) == hub_id))
            .cloned()
            .ok_or(PortError::NotFound)
    }

    fn account_id(&self, id: &str) -> String {
        format!("{}:{id}", self.descriptor.id)
    }

    async fn auth_file_name(&self, token: &str, hub_id: &str) -> PortResult<String> {
        let files = self.inventory(token).await?;
        rows(&files["files"])
            .iter()
            .find_map(|row| {
                let name = str_field(&row["name"])?;
                let id = str_field(&row["id"]).unwrap_or(name);
                (self.matches_family(row) && self.account_id(id) == hub_id).then(|| name.to_owned())
            })
            .ok_or(PortError::NotFound)
    }

    async fn config(&self, token: &str) -> PortResult<(Value, HeaderValue)> {
        let response =
            request(self.http.get(self.url("/config")).bearer_auth(token), "proxy config read", false, Duration::from_secs(10)).await?;
        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .filter(|etag| etag.as_bytes().starts_with(b"\"") && etag.as_bytes().len() > 2 && etag.as_bytes().ends_with(b"\""))
            .cloned()
            .ok_or_else(|| PortError::Unsupported("proxy config writes require an upstream ETag".into()))?;
        let config = json_body(response, "proxy config read").await?;
        if !config.is_object() {
            return Err(PortError::Unreachable("proxy config read: missing configuration object".into()));
        }
        Ok((config, etag))
    }

    async fn submit_command(&self, intent: &ProviderIntent, command: ProviderCommand) -> PortResult<ProviderDispatch> {
        let token = self.token().await?;
        match command {
            ProviderCommand::Quota(command) => self.submit_quota(&token, intent, command).await,
            ProviderCommand::Account(command) => self.submit_account(&token, command).await,
            ProviderCommand::Credential(CredentialCommand::RemoveLogin { account_id }) => {
                let name = self.auth_file_name(&token, &account_id).await?;
                let mut url = self.url(self.credential_path());
                url.query_pairs_mut().append_pair("name", &name);
                self.send(&token, Method::DELETE, url, None, None).await?;
                completed(json!({"removed": true}))
            }
            ProviderCommand::Credential(CredentialCommand::RenewLogin { account_id }) => {
                let names = if let Some(id) = account_id.as_deref().or(intent.target_account_id.as_deref()) {
                    vec![self.auth_file_name(&token, id).await?]
                } else {
                    let files = self.inventory(&token).await?;
                    rows(&files["files"])
                        .iter()
                        .filter(|row| self.matches_family(row))
                        .filter_map(|row| str_field(&row["name"]).map(str::to_owned))
                        .collect()
                };
                let mut renewed = 0;
                for name in names {
                    if let Err(error) = self.update(&token, Method::POST, "/credentials/refresh", json!({"name": name})).await {
                        return if renewed == 0 {
                            Err(error)
                        } else {
                            Err(ambiguous("proxy credential renewal may only be partially applied"))
                        };
                    }
                    renewed += 1;
                }
                completed(json!({"renewed": renewed}))
            }
            ProviderCommand::Routing(command) => self.submit_routing(&token, command).await,
            ProviderCommand::KeyGroup(command) => self.update_groups(&token, ProviderCommand::KeyGroup(command)).await,
            ProviderCommand::Model(ModelCommand::DiscoverModels { group_id }) => self.discover(&token, group_id.as_deref()).await,
            ProviderCommand::Model(command) => self.update_groups(&token, ProviderCommand::Model(command)).await,
            _ => Err(PortError::Unsupported("proxy operation unavailable".into())),
        }
    }

    async fn reconcile_intent(&self, _: &ProviderIntent) -> PortResult<ProviderReconciliation> {
        Ok(ProviderReconciliation::Unknown {
            reason: "proxy does not expose an operation lookup; inspect fresh state before retrying".into(),
        })
    }

    async fn submit_quota(&self, token: &str, intent: &ProviderIntent, command: QuotaCommand) -> PortResult<ProviderDispatch> {
        match command {
            QuotaCommand::RefreshQuota { account_id } | QuotaCommand::RefreshGrants { account_id } => {
                let ids = if let Some(id) = account_id.as_deref().or(intent.target_account_id.as_deref()) {
                    vec![auth_index(&self.quota_account(token, id).await?)?.to_owned()]
                } else {
                    // The unscoped upstream refresh also touches other tenants of this proxy.
                    let usage = self.quota(token).await?;
                    rows(&usage["credentials"])
                        .iter()
                        .filter(|row| self.matches_family(row) && row["disabled"].as_bool() != Some(true) && !row["api_key"].is_object())
                        .filter_map(|row| auth_index(row).ok().map(str::to_owned))
                        .collect()
                };
                let mut refreshed = 0;
                for index in ids {
                    let reply = self.update(token, Method::POST, "/usage/refresh", json!({"auth_index": index})).await;
                    let reply = match reply {
                        Err(error) if refreshed == 0 => return Err(error),
                        Err(_) => return Err(ambiguous("proxy quota refresh was partially applied")),
                        Ok(response) => mutation_body(response).await?,
                    };
                    if reply["errors"].as_object().is_some_and(|errors| !errors.is_empty()) {
                        return Err(rejected(502, "proxy reported a quota refresh failure"));
                    }
                    refreshed += 1;
                }
                completed(json!({"refreshed": refreshed}))
            }
            QuotaCommand::ClaimResetGrant { account_id, grant_id } => {
                let row = self.quota_account(token, &account_id).await?;
                let index = auth_index(&row)?;
                let credits = &row["reset_credits"];
                let grant = reset_grants(credits, Utc::now())
                    .into_iter()
                    .find(|grant| grant.grant_id == grant_id && grant.applicable)
                    .ok_or_else(|| rejected(409, "selected reset grant is not applicable"))?;
                let mut payload = json!({"auth_index": index});
                if credits.get("grants").is_some() {
                    payload["grant_id"] = json!(grant.grant_id);
                } else if self.descriptor.family != "codex" {
                    return Err(PortError::Unsupported("proxy must expose an explicit grant for this provider".into()));
                }
                let reply = mutation_body(self.update(token, Method::POST, "/usage/reset-credit", payload).await?).await?;
                match reply["code"].as_str() {
                    Some("reset") => completed(
                        json!({"reset_applied": true, "windows_reset": uint(&reply["windows_reset"]), "refresh_failed": !reply["refresh_error"].is_null()}),
                    ),
                    Some("no_credit" | "nothing_to_reset" | "already_redeemed") => {
                        Err(rejected(409, "proxy did not spend the selected reset grant"))
                    }
                    _ => Err(ambiguous("proxy reset grant outcome was not recognized")),
                }
            }
            QuotaCommand::TriggerRollingStart { account_id, model_id } => {
                let model =
                    model_id.filter(|id| !id.trim().is_empty()).ok_or_else(|| rejected(400, "proxy rolling start requires a model ID"))?;
                let row = self.quota_account(token, &account_id).await?;
                let index = auth_index(&row)?;
                self.update(token, Method::POST, "/usage/start-window", json!({"auth_index": index, "model": model})).await?;
                completed(json!({"window_started": true}))
            }
            QuotaCommand::ConfigureRollingStarter { .. } => {
                Err(PortError::Unsupported("proxy has no upstream rolling-window scheduler".into()))
            }
        }
    }

    async fn submit_account(&self, token: &str, command: AccountCommand) -> PortResult<ProviderDispatch> {
        let (suffix, mut body, id) = match command {
            AccountCommand::SetEnabled { account_id, enabled } => ("/status", json!({"disabled": !enabled}), account_id),
            AccountCommand::SetPriority { account_id, priority } => ("/fields", json!({"priority": priority}), account_id),
            AccountCommand::SetNote { account_id, note } => ("/fields", json!({"note": note}), account_id),
            AccountCommand::SetWeight { account_id, weight } => ("/fields", json!({"weight": weight}), account_id),
            AccountCommand::SetCapacity { account_id, plan, tier_multiplier } => {
                ("/fields", json!({"plan": plan, "tier_multiplier": tier_multiplier}), account_id)
            }
            _ => return Err(PortError::Unsupported("proxy account operation unavailable".into())),
        };
        let name = self.auth_file_name(token, &id).await?;
        body["name"] = json!(name);
        self.update(token, Method::PATCH, &format!("{}{suffix}", self.credential_path()), body).await?;
        completed(json!({"applied": true}))
    }

    async fn submit_routing(&self, token: &str, command: RoutingCommand) -> PortResult<ProviderDispatch> {
        match command {
            RoutingCommand::SetPoolOverride { account_id } => {
                let index = match account_id {
                    Some(id) => auth_index(&self.quota_account(token, &id).await?)?.to_owned(),
                    None => String::new(),
                };
                self.update(
                    token,
                    Method::PUT,
                    "/routing/preferred-auth",
                    json!({"provider": self.descriptor.family, "auth_index": index}),
                )
                .await?;
                completed(json!({"applied": true, "override_enabled": !index.is_empty()}))
            }
            RoutingCommand::MoveConversation { session_id, target_account_id, include_subagents } => {
                if !include_subagents {
                    return Err(PortError::Unsupported("proxy moves always include subagents".into()));
                }
                let row = self.quota_account(token, &target_account_id).await?;
                let reply = self
                    .update(
                        token,
                        Method::POST,
                        "/routing/move-session",
                        json!({"session_id": session_id, "auth_index": auth_index(&row)?}),
                    )
                    .await?;
                let reply = mutation_body(reply).await?;
                completed(json!({"applied": true, "bindings_moved": uint(&reply["bound"])}))
            }
            RoutingCommand::SetRoutePolicy { policy } => {
                let strategy = match policy.strategy {
                    RouteStrategy::TierAscending => "tier-ascending",
                    RouteStrategy::CapacityBalanced => "capacity-balanced",
                    RouteStrategy::SoonestReset => "soonest-reset",
                    RouteStrategy::FillFirst => "fill-first",
                    RouteStrategy::RoundRobin => "round-robin",
                    RouteStrategy::WeightedRoundRobin => "weighted-round-robin",
                };
                let defaults = RoutingPolicy::default();
                if policy.preferred_account_id.is_some()
                    || policy.switch_on_server_error != defaults.switch_on_server_error
                    || policy.failover_cooldown_secs != defaults.failover_cooldown_secs
                    || policy.near_reset_wait_secs != 0
                    || policy.first_byte_timeout_secs != defaults.first_byte_timeout_secs
                {
                    return Err(PortError::Unsupported("proxy routing policy contains fields not writable by this adapter".into()));
                }
                if self.v8 {
                    if ![0, defaults.session_affinity_secs].contains(&policy.session_affinity_secs) {
                        return Err(PortError::Unsupported("proxy routing supports only an affinity toggle".into()));
                    }
                    let key_order = match policy.api_key_order {
                        ApiKeyOrder::KeysFirst => "api-key",
                        ApiKeyOrder::KeysLast => "oauth",
                        ApiKeyOrder::SubscriptionsOnly => {
                            return Err(PortError::Unsupported("proxy cannot exclude API keys via this policy".into()));
                        }
                    };
                    let (_, etag) = self.config(token).await?;
                    let patch =
                        json!({"strategy": strategy, "session-affinity": policy.session_affinity_secs > 0, "prefer-auth-kind": key_order});
                    self.send(token, Method::PATCH, self.url("/config/routing"), Some(&patch), Some(etag)).await?;
                } else {
                    if policy.api_key_order != defaults.api_key_order || policy.session_affinity_secs != defaults.session_affinity_secs {
                        return Err(PortError::Unsupported("legacy proxy only supports changing the routing strategy".into()));
                    }
                    self.update(token, Method::PUT, "/routing/strategy", json!({"value": strategy})).await?;
                }
                completed(json!({"applied": true}))
            }
            _ => Err(PortError::Unsupported("proxy routing operation unavailable".into())),
        }
    }

    async fn update_groups(&self, token: &str, command: ProviderCommand) -> PortResult<ProviderDispatch> {
        let family =
            config_family(&self.descriptor.family).ok_or_else(|| PortError::Unsupported("proxy key-group family is unsupported".into()))?;
        let (config, etag) = self.config(token).await?;
        let configured_groups = &config["api-keys"][family];
        if (!config["api-keys"].is_null() && !config["api-keys"].is_object())
            || (!configured_groups.is_null() && !configured_groups.is_array())
        {
            return Err(PortError::Unreachable("proxy config read: invalid key group layout".into()));
        }
        let mut groups = rows(configured_groups).to_vec();
        let find = |groups: &[Value], id: &str| {
            groups
                .iter()
                .position(|group| str_field(&group["name"]).is_some_and(|name| self.group_id(family, name) == id))
                .ok_or(PortError::NotFound)
        };
        let mut result_id = None;
        match command {
            ProviderCommand::KeyGroup(KeyGroupCommand::UpsertGroup {
                group_id,
                family: requested_family,
                name,
                base_url: origin,
                enabled,
                request_identity,
                configured_models,
                aliases,
            }) => {
                if requested_family != family || name.trim() != name || name.is_empty() {
                    return Err(rejected(400, "proxy key group requires a matching family and nonempty name"));
                }
                if request_identity != RequestIdentityMode::Api && family != "claude" {
                    return Err(PortError::Unsupported("proxy request identity is only configurable for Claude".into()));
                }
                let origin = origin.as_deref().map(base_url).transpose()?.map(|url| url.to_string());
                let index = group_id.as_deref().map(|id| find(&groups, id)).transpose()?;
                if groups.iter().enumerate().any(|(i, g)| Some(i) != index && str_field(&g["name"]) == Some(name.as_str())) {
                    return Err(rejected(409, "proxy key group name already exists"));
                }
                let mut fresh = json!({"keys": []});
                let group = index.map_or(&mut fresh, |i| &mut groups[i]);
                group["name"] = json!(name);
                group["disabled"] = json!(!enabled);
                if let Some(origin) = origin {
                    group["base-url"] = json!(origin);
                } else if let Some(fields) = group.as_object_mut() {
                    fields.remove("base-url");
                }
                set_models(group, configured_models, aliases);
                if family == "claude" {
                    set_identity(group, request_identity);
                }
                result_id = Some(self.group_id(family, &name));
                if index.is_none() {
                    groups.push(fresh);
                }
            }
            ProviderCommand::KeyGroup(KeyGroupCommand::DeleteGroup { group_id }) => {
                let index = find(&groups, &group_id)?;
                groups.remove(index);
            }
            ProviderCommand::KeyGroup(KeyGroupCommand::SetGroupEnabled { group_id, enabled }) => {
                let index = find(&groups, &group_id)?;
                groups[index]["disabled"] = json!(!enabled);
            }
            ProviderCommand::KeyGroup(KeyGroupCommand::ToggleKey { group_id, key_id, enabled }) => {
                let index = find(&groups, &group_id)?;
                let key = groups[index]["keys"]
                    .as_array_mut()
                    .and_then(|keys| {
                        keys.iter_mut()
                            .find(|key| str_field(&key["api-key"]).is_some_and(|secret| fingerprint("spillway-key", secret) == key_id))
                    })
                    .ok_or(PortError::NotFound)?;
                key["disabled"] = json!(!enabled);
            }
            ProviderCommand::KeyGroup(KeyGroupCommand::SetRequestIdentity { group_id, mode }) => {
                let index = find(&groups, &group_id)?;
                set_identity(&mut groups[index], mode);
            }
            ProviderCommand::Model(ModelCommand::SetModels { group_id, models }) => {
                let index = find(&groups, &group_id)?;
                let aliases = group_models(&groups[index]).1.into_iter().filter(|a| models.contains(&a.target_model_id)).collect();
                set_models(&mut groups[index], models, aliases);
            }
            ProviderCommand::Model(ModelCommand::SetAliases { group_id, aliases }) => {
                let index = find(&groups, &group_id)?;
                let models = group_models(&groups[index]).0;
                set_models(&mut groups[index], models, aliases);
            }
            _ => return Err(PortError::Unsupported("proxy key operation unavailable".into())),
        }
        self.send(token, Method::PUT, self.url(&format!("/config/api-keys/{family}")), Some(&json!(groups)), Some(etag)).await?;
        completed(json!({"applied": true, "group_id": result_id}))
    }

    fn group_id(&self, family: &str, name: &str) -> String {
        format!("{}:{family}:{}", self.descriptor.id, fingerprint("spillway-group", name))
    }

    async fn discover(&self, token: &str, group_id: Option<&str>) -> PortResult<ProviderDispatch> {
        let mut models = BTreeMap::<String, DiscoveredModel>::new();
        if let Some(id) = group_id {
            if !self.v8 {
                return Err(PortError::Unsupported("legacy proxy does not expose grouped models".into()));
            }
            let config = self.get(token, self.url("/config")).await?;
            let groups = self.groups(&config, Utc::now());
            let group = groups.iter().find(|group| group.id == id).ok_or(PortError::NotFound)?;
            let body = self.get(token, self.url(&format!("/routing/model-definitions/{}", group.family))).await?;
            for raw in rows(&body["models"]) {
                if let Some(model) = str_field(&raw["id"]) {
                    models.insert(
                        model.into(),
                        discovered(
                            &self.descriptor,
                            model,
                            str_field(&raw["display_name"]).or_else(|| str_field(&raw["name"])),
                            Some(id),
                            Utc::now(),
                        ),
                    );
                }
            }
        } else {
            let files = self.inventory(token).await?;
            for file in rows(&files["files"]).iter().filter(|file| self.matches_family(file)) {
                let Some(name) = str_field(&file["name"]) else { continue };
                let mut url = self.url(&format!("{}/models", self.credential_path()));
                url.query_pairs_mut().append_pair("name", name);
                let body = self.get(token, url).await?;
                for raw in rows(&body["models"]) {
                    if let Some(id) = str_field(&raw["id"]) {
                        models.insert(id.into(), discovered(&self.descriptor, id, str_field(&raw["display_name"]), None, Utc::now()));
                    }
                }
            }
        }
        let count = models.len();
        let mut cache = self.discovered.write().await;
        cache.retain(|model| model.group_id.as_deref() != group_id);
        cache.extend(models.into_values());
        completed(
            json!({"models_discovered": count, "source": if group_id.is_some() { "proxy_model_definitions" } else { "proxy_inventory" }}),
        )
    }

    fn groups(&self, config: &Value, at: DateTime<Utc>) -> Vec<KeyGroup> {
        let Some(family) = config_family(&self.descriptor.family) else { return Vec::new() };
        rows(&config["api-keys"][family])
            .iter()
            .filter_map(|raw| {
                let name = str_field(&raw["name"])?;
                let (configured_models, aliases) = group_models(raw);
                let keys: Vec<_> = rows(&raw["keys"])
                    .iter()
                    .enumerate()
                    .filter_map(|(index, key)| {
                        let secret = str_field(&key["api-key"])?;
                        let hint = if secret.len() >= 12 {
                            format!("••••{}", secret.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect::<String>())
                        } else {
                            "••••".into()
                        };
                        Some(MaskedApiKey {
                            id: fingerprint("spillway-key", secret),
                            label: format!("Key {}", index + 1),
                            key_hint: hint,
                            enabled: key["disabled"].as_bool() != Some(true),
                            created_at: at,
                            last_used_at: None,
                            success_count: 0,
                            failure_count: 0,
                        })
                    })
                    .collect();
                Some(KeyGroup {
                    id: self.group_id(family, name),
                    provider_id: self.descriptor.id.clone(),
                    family: family.into(),
                    name: name.into(),
                    base_url: str_field(&raw["base-url"]).and_then(|url| base_url(url).ok()).map(|url| url.to_string()),
                    enabled: raw["disabled"].as_bool() != Some(true),
                    request_identity: if family == "claude"
                        && rows(&raw["keys"]).iter().any(|key| key["cloak"]["mode"].as_str() == Some("auto"))
                    {
                        RequestIdentityMode::AutoClient
                    } else {
                        RequestIdentityMode::Api
                    },
                    keys,
                    configured_models,
                    discovered_models: Vec::new(),
                    aliases,
                    updated_at: at,
                })
            })
            .collect()
    }
}

typed_facets!(SpillwayProxyProvider:
    quota => QuotaCommand, Quota, QUOTA;
    accounts => AccountCommand, Account, ACCOUNTS;
    credentials => CredentialCommand, Credential, CREDENTIALS;
    key_groups => KeyGroupCommand, KeyGroup, KEY_GROUPS;
    models => ModelCommand, Model, MODELS;
    routing => RoutingCommand, Routing, ROUTING;
);

#[async_trait]
impl ModelProvider for SpillwayProxyProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        self.descriptor.clone()
    }
    fn facets(&self) -> ProviderFacets<'_> {
        self.command_facets()
    }
    async fn collect(&self) -> PortResult<Option<ObservationBatch>> {
        let token = self.token().await?;
        let strategy_path = if self.v8 { "/config/routing/strategy" } else { "/routing/strategy" };
        let config_path = if self.v8 { "/config" } else { "/debug" };
        let log_path = if self.v8 { "/observability/logs" } else { "/logs" };
        let mut logs_url = self.url(log_path);
        logs_url.query_pairs_mut().append_pair("limit", "100");
        let quota = async { if self.v8 { self.quota(&token).await } else { Ok(Value::Null) } };
        let strategy = async {
            let response =
                request(self.http.get(self.url(strategy_path)).bearer_auth(&token), "proxy status", false, Duration::from_secs(10)).await?;
            let version = response
                .headers()
                .get("x-cpa-version")
                .and_then(|value| value.to_str().ok())
                .filter(|s| !s.is_empty() && s.len() <= 64 && s.bytes().all(|c| c.is_ascii_alphanumeric() || b".-+_".contains(&c)))
                .map(str::to_owned);
            Ok::<_, PortError>((json_body(response, "proxy status").await?, version))
        };
        let (files, usage, strategy, config, logs) =
            tokio::join!(self.inventory(&token), quota, strategy, self.get(&token, self.url(config_path)), self.get(&token, logs_url),);
        let files = match files {
            Err(error) if usage.is_err() || !self.v8 => return Err(error),
            other => other,
        };
        let at = Utc::now();
        let mut accounts = BTreeMap::new();
        let files_available = files.is_ok();
        let quota_available = usage.is_ok();
        let usage = usage.unwrap_or_default();
        let files = files.unwrap_or_default();
        for (raw, quota_row) in rows(&files["files"])
            .iter()
            .map(|row| (row, false))
            .chain(rows(&usage["credentials"]).iter().map(|row| (row, true)))
            .filter(|(row, _)| self.matches_family(row))
        {
            let Some(id) = str_field(&raw["id"]).or_else(|| str_field(&raw["name"])) else { continue };
            let account = accounts.entry(id).or_insert_with(|| account(&self.descriptor, id, id));
            read_account(account, raw, quota_row, at);
        }
        let grants = rows(&usage["credentials"])
            .iter()
            .filter(|r| self.matches_family(r))
            .filter_map(|row| {
                let id = str_field(&row["id"])?;
                Some((self.account_id(id), reset_grants(&row["reset_credits"], at)))
            })
            .flat_map(|(id, grants)| grants.into_iter().map(move |grant| (id.clone(), grant)))
            .collect::<Vec<_>>();
        let mut batch = batch(&self.descriptor, "spillway", accounts.into_values().collect(), at);
        batch.source_publisher = Some("spillway-proxy".into());
        batch.reset_grants = grants;
        if batch.accounts.is_empty() {
            batch.health = ProviderHealth::Healthy;
        }
        if !files_available || !quota_available {
            batch.health = ProviderHealth::Degraded;
            batch.status_message = Some("proxy returned only partial account evidence".into());
        }
        let config = config.unwrap_or_default();
        let (strategy, build_version) = strategy.unwrap_or_default();
        let route_strategy = [
            str_field(&strategy),
            str_field(&strategy["strategy"]),
            str_field(&usage["strategy"]),
            str_field(&config["routing"]["strategy"]),
        ]
        .into_iter()
        .flatten()
        .find_map(parse_route_strategy)
        .unwrap_or_default();
        let policy = RoutingPolicy {
            strategy: route_strategy,
            session_affinity_secs: if config["routing"]["session-affinity"].as_bool() == Some(false) {
                0
            } else {
                RoutingPolicy::default().session_affinity_secs
            },
            api_key_order: if str_field(&config["routing"]["prefer-auth-kind"]) == Some("api-key") {
                ApiKeyOrder::KeysFirst
            } else {
                ApiKeyOrder::KeysLast
            },
            preferred_account_id: batch.accounts.iter().find(|a| a.is_preferred).map(|a| a.id.clone()),
            ..RoutingPolicy::default()
        };
        let debug_logs = config["debug"].as_bool().or_else(|| config["logging"]["debug"].as_bool()).unwrap_or(false);
        let router_id = self.descriptor.router_id.clone().expect("proxy router ID");
        let proxy_logs = read_logs(&logs.unwrap_or_default());
        batch.routing_status = Some(RoutingView {
            provider_id: self.descriptor.id.clone(),
            router_id: Some(router_id.clone()),
            revision: 0,
            runtime: Some(ProxyRuntimeStatus {
                router_id,
                running: true,
                endpoint: None,
                port: None,
                lan_bind: false,
                auto_start: false,
                debug_logs,
                build_version,
                client_key_hint: None,
                shared_profile_owner: None,
                crash_restarts: 0,
                last_error: None,
                updated_at: at,
            }),
            policy,
            clients: Vec::new(),
            lockouts: Vec::new(),
            recent_logs: proxy_logs,
            rolling_starter: RollingWindowStarterConfig::default(),
        });
        batch.key_groups = self.groups(&config, at);
        batch.models = self.discovered.read().await.clone();
        for group in &mut batch.key_groups {
            group.discovered_models = batch.models.iter().filter(|m| m.group_id.as_ref() == Some(&group.id)).cloned().collect();
        }
        Ok(Some(batch))
    }
}

fn parse_route_strategy(raw: &str) -> Option<RouteStrategy> {
    Some(match raw {
        "round-robin" | "round_robin" => RouteStrategy::RoundRobin,
        "weighted-round-robin" | "weighted_round_robin" => RouteStrategy::WeightedRoundRobin,
        "fill-first" | "fill_first" => RouteStrategy::FillFirst,
        "soonest-reset" | "soonest_reset" => RouteStrategy::SoonestReset,
        "tier-ascending" | "tier_ascending" => RouteStrategy::TierAscending,
        "capacity-balanced" | "capacity_balanced" => RouteStrategy::CapacityBalanced,
        _ => return None,
    })
}

fn auth_index(row: &Value) -> PortResult<&str> {
    str_field(&row["auth_index"]).ok_or_else(|| rejected(409, "proxy has not supplied an auth index"))
}

fn completed(receipt: Value) -> PortResult<ProviderDispatch> {
    Ok(ProviderDispatch::Completed { receipt })
}

fn rejected(status: u16, message: &'static str) -> PortError {
    PortError::Rejected { status, message: message.into() }
}

fn ambiguous(message: &'static str) -> PortError {
    PortError::Ambiguous(message.into())
}

async fn mutation_body(response: Response) -> PortResult<Value> {
    json_body(response, "proxy management update").await.map_err(|_| ambiguous("proxy applied status could not be read"))
}

fn config_family(family: &str) -> Option<&str> {
    match family {
        "claude" | "codex" | "gemini" | "vertex" | "gemini_interactions" | "xai" | "meta" | "openai_compat" => Some(family),
        _ => None,
    }
}

fn group_models(group: &Value) -> (Vec<String>, Vec<ModelAlias>) {
    let mut models = BTreeSet::new();
    let mut aliases = Vec::new();
    for model in rows(&group["models"]) {
        let Some(name) = str_field(&model["name"]).or_else(|| str_field(model)) else { continue };
        models.insert(name.to_owned());
        if let Some(alias) = str_field(&model["alias"]) {
            aliases.push(ModelAlias { alias: alias.into(), target_model_id: name.into() });
        }
    }
    (models.into_iter().collect(), aliases)
}

fn set_models(group: &mut Value, models: Vec<String>, aliases: Vec<ModelAlias>) {
    let mut entries: BTreeMap<(String, Option<String>), Value> = BTreeMap::new();
    for raw in rows(&group["models"]) {
        if let Some(name) = str_field(&raw["name"]) {
            entries.insert((name.into(), str_field(&raw["alias"]).map(str::to_owned)), raw.clone());
        }
    }
    let model_names: BTreeSet<_> = models.into_iter().chain(aliases.iter().map(|a| a.target_model_id.clone())).collect();
    let mut next = Vec::new();
    for name in model_names {
        let mut found = false;
        for alias in aliases.iter().filter(|alias| alias.target_model_id == name) {
            found = true;
            next.push(
                entries.remove(&(name.clone(), Some(alias.alias.clone()))).unwrap_or_else(|| json!({"name": name, "alias": alias.alias})),
            );
        }
        if !found {
            next.push(entries.remove(&(name.clone(), None)).unwrap_or_else(|| json!({"name": name})));
        }
    }
    group["models"] = json!(next);
}

fn set_identity(group: &mut Value, mode: RequestIdentityMode) {
    if let Some(keys) = group["keys"].as_array_mut() {
        for key in keys.iter_mut().filter_map(Value::as_object_mut) {
            key.remove("fingerprint-profile");
            let mode = if mode == RequestIdentityMode::AutoClient { "auto" } else { "uncloak" };
            if let Some(cloak) = key.get_mut("cloak").and_then(Value::as_object_mut) {
                cloak.insert("mode".into(), json!(mode));
            } else {
                key.insert("cloak".into(), json!({"mode": mode}));
            }
        }
    }
}

fn read_account(account: &mut ModelAccount, raw: &Value, quota_row: bool, now: DateTime<Utc>) {
    if let Some(label) = str_field(&raw["label"]) {
        account.display_name = label.into();
    }
    if let Some(email) = str_field(&raw["email"]) {
        account.masked_email = Some(mask_email(email));
    }
    if let Some(note) = str_field(&raw["note"]) {
        account.note = Some(note.into());
    }
    if let Some(disabled) = raw["disabled"].as_bool() {
        account.enabled = !disabled;
    }
    if let Some(priority) = raw["priority"].as_i64().and_then(|n| i32::try_from(n).ok()) {
        account.priority = priority;
    }
    if let Some(weight) = uint(&raw["weight"]) {
        account.weight = weight;
    }
    if let Some(last_used_at) = iso(&raw["last_used_at"]) {
        account.last_used_at = Some(last_used_at);
    }
    if let Some(error) = raw.get("last_error") {
        let parsed = serde_json::from_value::<AccountError>(error.clone()).ok().map(AccountError::sanitized);
        if parsed.is_some() || error.is_null() {
            account.last_error = parsed;
            account.last_error_observed = true;
        }
    }
    let legacy_error = account.last_error.is_none()
        && match raw["status"].as_str() {
            Some("error" | "unavailable") => true,
            None if raw.get("last_error").is_none() => account.enrollment_state == EnrollmentState::Error,
            _ => false,
        };
    account.status_message =
        account.last_error.as_ref().map(AccountError::summary).or_else(|| legacy_error.then(|| "proxy reports a credential error".into()));
    account.enrollment_state = if legacy_error { EnrollmentState::Error } else { EnrollmentState::Deployed };
    if let Some(multiplier) = [raw.pointer("/plan/multiplier"), raw.get("tier_multiplier")]
        .into_iter()
        .flatten()
        .filter_map(Value::as_f64)
        .find(|value| value.is_finite() && *value > 0.0)
    {
        account.tier_multiplier = Some(multiplier);
    }
    if quota_row {
        account.plan = str_field(&raw["plan"]["label"]).or_else(|| str_field(&raw["plan"]["tier"])).map(str::to_owned);
        account.is_preferred = raw["preferred"].as_bool() == Some(true);
        account.is_next_pick = raw["next_pick"].as_bool() == Some(true);
        account.cooldown_until = iso(&raw["cooldown_until"]);
        account.last_observed_at = iso(&raw["observed_at"]);
        if account.last_observed_at.is_some() && !legacy_error {
            account.enrollment_state = EnrollmentState::Verified;
        }
        account.success_count = raw["api_key"]["success"].as_u64().unwrap_or_default();
        account.failure_count = raw["api_key"]["failed"].as_u64().unwrap_or_default();
        account.windows = rows(&raw["windows"]).iter().filter_map(|window| read_window(window, account.last_observed_at, now)).collect();
        account.reset_grant = reset_grants(&raw["reset_credits"], now).into_iter().max_by_key(|g| (g.applicable, g.available_count > 0));
        if raw["api_key"].is_object() {
            for cap in [
                ProviderCapability::RefreshQuota,
                ProviderCapability::RefreshGrants,
                ProviderCapability::ClaimResetGrant,
                ProviderCapability::StartRollingWindow,
                ProviderCapability::RenewLogin,
                ProviderCapability::RemoveLogin,
                ProviderCapability::SetEnabled,
                ProviderCapability::SetNote,
                ProviderCapability::SetPriority,
                ProviderCapability::SetWeight,
                ProviderCapability::SetCapacity,
            ] {
                account.capabilities.remove(&cap);
            }
        }
    }
    account.status = AccountStatus::resolve_with_error(
        account.enabled,
        account.cooldown_until,
        now,
        account.is_next_pick,
        legacy_error,
        account.last_error.as_ref(),
    );
}

fn read_window(raw: &Value, account_time: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Option<QuotaWindow> {
    let key = str_field(&raw["key"])?;
    let mut window = percent_window(key, str_field(&raw["label"]).unwrap_or(key), raw["used_percent"].as_f64(), now);
    window.observed_at = iso(&raw["observed_at"]).or(account_time);
    window.stale = raw["stale"].as_bool() == Some(true);
    if window.stale && window.used_percent.is_some() {
        window.observation_state = QuotaObservationState::StaleLastGood;
    } else if window.observed_at.is_none() && window.used_percent.is_none() {
        window.observation_state = QuotaObservationState::NeverObserved;
    }
    window.resets_at = iso(&raw["resets_at"]);
    window.window_duration_mins = uint(&raw["duration_seconds"]).filter(|n| *n >= 60).map(|n| n / 60);
    window.cadence = match raw["reset_cadence"].as_str() {
        Some("on_use") => ResetCadence::RollingOnUse,
        Some("fixed") => ResetCadence::Fixed,
        _ => ResetCadence::Unknown,
    };
    window.kind = if raw["scoped"].as_bool() == Some(true) {
        QuotaWindowKind::ModelScoped
    } else {
        match window.window_duration_mins {
            Some(300) => QuotaWindowKind::Session,
            Some(1440) => QuotaWindowKind::Daily,
            Some(10080) => QuotaWindowKind::Weekly,
            _ => QuotaWindowKind::Custom,
        }
    };
    window.status = match raw["status"].as_str() {
        Some("allowed") => QuotaStatus::Allowed,
        Some("allowed_warning") => QuotaStatus::Warning,
        Some("rejected") => QuotaStatus::Rejected,
        _ => QuotaStatus::Unknown,
    };
    Some(window)
}

fn discovered(provider: &ProviderDescriptor, id: &str, label: Option<&str>, group_id: Option<&str>, at: DateTime<Utc>) -> DiscoveredModel {
    DiscoveredModel {
        id: id.into(),
        display_name: label.map(str::to_owned),
        family: Some(provider.family.clone()),
        provider_id: provider.id.clone(),
        group_id: group_id.map(str::to_owned),
        context_window: None,
        supports_thinking: None,
        discovered_at: Some(at),
    }
}

fn read_logs(body: &Value) -> Vec<ProxyLogEntry> {
    rows(&body["lines"])
        .iter()
        .filter_map(|line| {
            let line = line.as_str()?;
            // Free-form proxy output can contain credentials and captured request bodies.
            // Keep only metadata from the log envelope, not arbitrary message text.
            let time = line.split_once(']')?.0.strip_prefix('[')?;
            let at =
                time.parse().ok().or_else(|| chrono::NaiveDateTime::parse_from_str(time, "%Y-%m-%d %H:%M:%S").ok().map(|t| t.and_utc()))?;
            let level = line
                .split('[')
                .skip(2)
                .take(2)
                .filter_map(|part| part.split_once(']').map(|(s, _)| s.trim()))
                .find(|s| ["error", "warn", "warning", "info", "debug", "trace"].contains(s))
                .unwrap_or("info");
            Some(ProxyLogEntry { at, stream: "proxy".into(), level: level.into(), message: "Proxy log entry (message omitted)".into() })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_payload_preserves_structured_account_error_and_can_clear_it() {
        let at = Utc::now();
        let d = descriptor("test", "Test", "claude", AdapterMode::Live);
        let mut a = account(&d, "primary", "Primary");
        let failure =
            json!({"kind":"auth_expired","source":"oauth_refresh","message":"Sign in again","occurred_at":at,"consecutive_count":2});
        read_account(&mut a, &json!({"status":"error","last_error":failure}), false, at);
        read_account(&mut a, &json!({"next_pick":true}), true, at);
        assert_eq!(a.status, AccountStatus::Error);
        assert_eq!(a.last_error.as_ref().unwrap().kind, AccountErrorKind::AuthExpired);
        assert_eq!(a.status_message.as_deref(), Some("Authentication expired: Sign in again"));
        read_account(&mut a, &json!({"next_pick":true,"observed_at":at,"last_error":null}), true, at);
        assert!(a.last_error.is_none());
        assert!(a.last_error_observed);
        assert!(a.status_message.is_none());
        assert_eq!(a.status, AccountStatus::NextPick);
    }
}
