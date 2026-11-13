# Fleetix Hub vision

Fleetix Hub is a local-first content operations hub. It combines an Axum API,
a transactional domain command layer, an integration worker, and a Dioxus
desktop console over a shared typed contract, with PostgreSQL migrations and a
registry of external services. Its purpose is to give one operator console for
content work that otherwise spans several tools, and to keep the evidence for
each integration honest about what was actually verified.

## Problem

Content production runs across several services — ComfyUI for generation,
Paperclip for issues and approvals, Windmill for jobs — each with its own API,
authentication, and failure modes. Wiring them per project produces scripts
that mix live and fixture behaviour, hide which requests were ever made, and
leave no audit trail from an operator action to its external effect.

## Principles

- **Local-first, single operator surface.** The desktop console, the API, and
  `hubctl` all speak the same typed client; the hub owns its database and asset
  store rather than delegating state to a cloud service.
- **Typed contracts at the boundary.** `crates/contracts` and the API's
  `openapi.json` define request and response shapes; the domain layer executes
  commands transactionally and hashes releases.
- **Fixtures are not evidence.** The capability report distinguishes built,
  fixture-tested, contract-verified, and live-verified adapters, and states that
  fixture adapters are lead-owned and were not run as evidence for the live
  adapters.
- **No mutating requests without a decision.** The recorded capability
  observations were read-only; issue and approval creation are catalogued as
  contract-verified rather than live-proven.
- **Donor provenance is explicit.** Reused shell, component, and stylesheet code
  comes from one pinned, MIT-licensed snapshot with file-level hashes, and no
  donor branding is copied.

## Scope

- `crates/domain`: command layer, ports, release hashing
- `crates/api`: Axum HTTP API and OpenAPI document
- `crates/integrations`: live and fixture adapters, asset store, GPU admission
- `crates/worker`: integration worker
- `crates/contracts`: shared wire types
- `apps/desktop`: Dioxus operator console with workspace navigation and
  inspector
- `crates/domain/migrations/` and `registry/` for persistence and external-service catalogues

## Direction

The current increment built the API, worker, fixture journey, and live and
fixture adapters, then migrated the desktop shell to Dioxus 0.8 and documented
capabilities, contracts, and source provenance. The next visible work is
turning contract-verified adapters into live-verified ones.

## Place in the fleet

No consumer was found in canix or the other owned repositories; the project is
currently developed as a standalone hub.
