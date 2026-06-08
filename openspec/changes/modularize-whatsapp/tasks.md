# Tasks: Modularize WhatsApp Module

## Review Workload Forecast

| Field | Value |
|---|---|
| Estimated changed lines | 650-850 |
| 400-line budget risk | High |
| Chained PRs recommended | Yes |
| Suggested split | PR1 shared helpers -> PR2 webhook/conversations -> PR3 messaging/settings -> PR4 quick replies/templates cleanup |
| Delivery strategy | force-chained |
| Chain strategy | feature-branch-chain |

Decision needed before apply: No
Chained PRs recommended: Yes
Chain strategy: feature-branch-chain
400-line budget risk: High

### Suggested Work Units

| Unit | Goal | Likely PR | Notes |
|---|---|---|---|
| 1 | Extract shared helpers + AI import migration | PR 1 | Base = tracker branch; move `shared/*`; verify `cargo check`; rollback = revert shared-only commit. |
| 2 | Extract webhook + conversations | PR 2 | Base = PR1 branch; move `webhook/*` and `conversations/*`; verify route inventory + OpenAPI parity for moved paths. |
| 3 | Extract messaging/media/settings | PR 3 | Base = PR2 branch; move `messaging/*` and `settings/*`; verify media/body-limit paths and schema parity. |
| 4 | Extract quick replies/templates + finalize facade cleanup | PR 4 | Base = PR3 branch; move `quick_replies/*` and `templates/*`; remove dead exports; final full-route/OpenAPI parity. |

## Phase 1: Foundation

- [x] 1.1 Create `src/modules/whatsapp/shared/{mod.rs,authz.rs,mappers.rs,response.rs,time.rs,workspace.rs}` and move pure helpers/builders from `handler.rs`.
- [x] 1.2 Update `src/modules/ai_agent/{dispatch.rs,escalation.rs}` to import `whatsapp::shared::mappers` instead of `handler`.
- [x] 1.3 Keep `handler.rs` as a compatibility facade with re-exports only; do not change external routes. _(PR1 added compatibility wrappers; PR4m completed the cleanup by reducing `src/modules/whatsapp/handler.rs` to a re-export-only legacy shim after webhook runtime ownership moved into `webhook::handler`.)_

## Phase 2: Domain Extraction

- [x] 2.1 Move webhook handlers into `webhook/{mod.rs,handler.rs,normalize.rs,media_failures.rs,status.rs}` and preserve `verify_webhook`, `receive_webhook`, `debug_last_webhook_handler`. _(PR2 created the module boundary, PR2b moved verify/debug ownership, and PR4m moved `receive_webhook` plus its remaining runtime helpers into `webhook::handler` while relocating normalization tests into `webhook::normalize`.)_
- [x] 2.2 Move conversation REST flows into `conversations/{mod.rs,handlers.rs,lifecycle.rs,queries.rs}`; keep route paths/order unchanged. _(Verified in PR4m: legacy `handler.rs` no longer owns conversation REST bodies. Route order stays in `src/modules/whatsapp/mod.rs` via `conversations::handlers`, which now fronts `queries`, `lifecycle`, and `outbound` ownership while `messaging::send` remains intentionally re-exported there from completed task 2.3.)_
- [x] 2.3 Move messaging/media/reaction code into `messaging/{mod.rs,send.rs,reactions.rs,media.rs,preview.rs}`. _(PR3i extracted inbound media-failure fallback to `webhook::media_failures`; messaging/media/reaction ownership is now out of `handler.rs`.)_
- [x] 2.4 Move settings + WhatsApp Numbers + test-connection into `settings/{mod.rs,handlers.rs,validation.rs}`. _(PR4a extracted validation helpers; PR4b moved handler ownership and rewired routes/OpenAPI to `settings::handlers` while preserving behavior.)_

## Phase 3: Wiring / Verification

- [x] 3.1 Update `src/modules/whatsapp/mod.rs` to wire the new modules without changing the route inventory from `user_routes`, `webhook_routes`, or `ws_routes`.
- [x] 3.2 Update `src/openapi.rs` path/component registrations per slice; verify semantic parity for every rewired WhatsApp endpoint.
- [x] 3.3 Add route inventory checks from `mod.rs` and OpenAPI diff checks from `/docs/openapi.json` for each PR slice. _(PR4k added `mod.rs` inventory parity tests plus generated OpenAPI path/tag/security checks, and fixed the missing audit route registrations those checks exposed.)_

## Phase 4: Cleanup / Final Parity

- [x] 4.1 Move quick replies into `quick_replies/{mod.rs,handlers.rs}` and templates into `templates/{mod.rs,handlers.rs,meta.rs,header_media.rs}`. (PR4c completed Quick Replies ownership. PR4d moved template list/get ownership into `templates::handlers`; PR4e moved template delete/resync ownership there; PR4f moved template create ownership there; PR4g moved template update ownership there and completed the template CRUD handler extraction boundary; PR4h added `templates/meta.rs` and moved shared template helper ownership there. PR4i added `templates/header_media.rs` and rewired template header-media route/OpenAPI ownership there while preserving the endpoint contract.)
- [x] 4.2 Trim `handler.rs` to a minimal legacy shim; remove dead exports only after PR4 OpenAPI parity passes. (PR4d removed template list/get bodies and kept compatibility re-exports; PR4e also removed delete/resync bodies; PR4f removed the create body; PR4g removed the update body; PR4h moved shared template helpers into `templates::meta` and removed the now-dead template handler compatibility exports; PR4j removed the temporary `map_meta_error` shim and dead quick-reply compatibility re-exports; PR4l moved `process_template_status` into `webhook::status`; PR4m moved the remaining webhook runtime into `webhook::handler` and left `src/modules/whatsapp/handler.rs` as a minimal compatibility shim with parity preserved.)
- [ ] 4.3 Final verification: `cargo check`, targeted WhatsApp regression tests, full route inventory, and OpenAPI parity against the pre-refactor contract.
