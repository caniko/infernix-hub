use super::*;
use async_trait::async_trait;
use domain::ports::{ModelProvider, ProviderDispatch, ProviderFacets, ProviderReconciliation};
use serde_json::json;

/// Credentials and execution remain with an authenticated user-session collector.
pub struct PushModelProvider {
    descriptor: ProviderDescriptor,
}

impl PushModelProvider {
    pub fn new(id: &str, label: &str, family: &str, capabilities: ProviderCapabilities, evidence_kinds: BTreeSet<EvidenceKind>) -> Self {
        let mut descriptor = descriptor(id, label, family, AdapterMode::Live);
        descriptor.capabilities = capabilities;
        descriptor.evidence_kinds = evidence_kinds;
        Self { descriptor }
    }

    async fn submit_command(&self, _: &ProviderIntent, _: ProviderCommand) -> PortResult<ProviderDispatch> {
        Ok(ProviderDispatch::Accepted { receipt: Some(json!({"queued_for_collector": true})) })
    }

    async fn reconcile_intent(&self, _: &ProviderIntent) -> PortResult<ProviderReconciliation> {
        Ok(ProviderReconciliation::StillPending)
    }
}

typed_facets!(PushModelProvider:
    quota => QuotaCommand, Quota, QUOTA;
    accounts => AccountCommand, Account, ACCOUNTS;
    credentials => CredentialCommand, Credential, CREDENTIALS;
    key_groups => KeyGroupCommand, KeyGroup, KEY_GROUPS;
    models => ModelCommand, Model, MODELS;
    routing => RoutingCommand, Routing, ROUTING;
    probes => ProbeCommand, Probe, PROBES;
);

#[async_trait]
impl ModelProvider for PushModelProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        self.descriptor.clone()
    }

    fn facets(&self) -> ProviderFacets<'_> {
        self.command_facets()
    }
}
