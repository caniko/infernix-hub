-- Remove non-inference empty-model zero-token exchanges and reconcile hourly totals.
with deleted_exchanges as (
    delete from activity_exchanges
    where btrim(effective_model) = ''
      and input_tokens = 0
      and output_tokens = 0
      and cache_read_tokens = 0
      and cache_write_tokens = 0
      and thinking_tokens = 0
    returning id,
              date_trunc('hour', occurred_at) as hour_bucket,
              source,
              provider_id,
              coalesce(account_id, '') as account_id,
              client,
              effective_model,
              coalesce(effort, '') as effort,
              purpose,
              outcome,
              api_equivalent_cost_usd
),
deleted_identities as (
    delete from activity_identities
    where canonical_exchange_id in (select id from deleted_exchanges)
),
bucket_deltas as (
    select hour_bucket,
           source,
           provider_id,
           account_id,
           client,
           effective_model,
           effort,
           count(*)::bigint as requests,
           count(*) filter (where outcome not in ('ok', 'succeeded', 'success'))::bigint as failures,
           count(*) filter (where api_equivalent_cost_usd is null)::bigint as unpriced_requests
    from deleted_exchanges
    where purpose = 'ordinary'
    group by hour_bucket, source, provider_id, account_id, client, effective_model, effort
)
update activity_totals t
set requests = greatest(t.requests - d.requests, 0),
    failures = greatest(t.failures - d.failures, 0),
    unpriced_requests = greatest(t.unpriced_requests - d.unpriced_requests, 0)
from bucket_deltas d
where t.hour_bucket = d.hour_bucket
  and t.source = d.source
  and t.provider_id = d.provider_id
  and t.account_id = d.account_id
  and t.client = d.client
  and t.effective_model = d.effective_model
  and t.effort = d.effort;

delete from activity_totals
where btrim(effective_model) = ''
  and (
      requests <= 0
      or (
          input_tokens = 0
          and output_tokens = 0
          and cache_read_tokens = 0
          and cache_write_tokens = 0
          and thinking_tokens = 0
      )
  );
