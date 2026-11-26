-- Membership of the latest Antigravity proxy report, distinct from retained inventory.
-- NULL means no membership report; an empty array means the proxy has no accounts.
alter table provider_routing_state
    add column routed_account_ids text[],
    add column routed_accounts_collected_at timestamptz;
