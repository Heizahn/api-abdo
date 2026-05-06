# Skill Registry — api-abdo

Generated: 2026-05-01
Project: api-abdo (Rust + Axum 0.7 ISP API)

## Project Conventions

- `CLAUDE.md` — Project context, stack, architecture patterns, module structure, OpenAPI workflow, env vars
- `~/.claude/CLAUDE.md` — Global user preferences (response length, language, Engram protocol, SDD orchestrator rules)
- `~/.claude/projects/C--Users-Humberto-Develop-api-abdo/memory/MEMORY.md` — Auto-memory index (project conventions, DB schema, AI Agent state)

## Project Skills (`.claude/skills/`)

| Skill | Triggers (when to inject) |
|-------|--------------------------|
| `rust-axum-framework` | Any change touching `src/**/*.rs` involving routing, extractors, middleware, state, or HTTP layer. Tags: axum, rust, web-framework, tokio, tower, async, rest-api |

## User Skills — Relevant for this project

| Skill | Triggers |
|-------|----------|
| `rust` | Writing/reviewing/refactoring Rust code — performance, ownership, borrowing, iterators, async, allocation |
| `rust-async-patterns` | Async Rust with Tokio, async traits, error handling, concurrent patterns |
| `rust-engineer` | Building Rust apps requiring memory safety, lifetimes, traits, async/await with tokio |
| `branch-pr` | Creating a PR, opening a pull request, preparing changes for review |
| `issue-creation` | Creating a GitHub issue, reporting a bug, requesting a feature |
| `simplify` | Reviewing changed code for reuse, quality, efficiency |
| `judgment-day` | User says "judgment day", "dual review", "doble review", "juzgar" — parallel adversarial review |
| `find-skills` | User asks "is there a skill for X" / "how do I do X" |
| `update-config` | Changes to `.claude/settings.json`, hooks, permissions, env vars |
| `fewer-permission-prompts` | Reducing permission prompts via allowlist |
| `loop` | Recurring tasks, polling, "every N minutes" |
| `schedule` | Scheduling remote agents (cron-style routines) |

## SDD Skills (Always available — orchestrator routes them)

| Skill | Phase |
|-------|-------|
| `sdd-init` | Project context detection + persistence |
| `sdd-explore` | Investigation before proposal |
| `sdd-propose` | Change proposal (intent, scope, approach) |
| `sdd-spec` | Delta specs with Given/When/Then |
| `sdd-design` | Technical design + architecture decisions |
| `sdd-tasks` | Task breakdown |
| `sdd-apply` | Implementation |
| `sdd-verify` | Validation against specs |
| `sdd-archive` | Merge deltas + archive change |

## Compact Rules (auto-resolved into sub-agent prompts)

### Rust + Axum (apply to any sub-agent touching `src/**/*.rs`)

```
## Project Standards (auto-resolved)

### Rust + Axum
- Feature modules: src/modules/<feature>/{mod.rs,handler.rs}. mod.rs exposes `pub fn routes() -> Router<Arc<AppState>>`.
- Register routes in src/axum_router.rs in the correct auth group (webhook | ws | public | user_protected | client_protected).
- DB access: define methods on traits in src/db/mod.rs, implement in src/db/mongo/. Avoid $lookup on large collections — prefer parallel queries + join in Rust.
- Errors: return ApiError. The IntoResponse impl produces { ok: false, error: "<code>" }.
- OpenAPI: every new handler MUST have #[utoipa::path(...)]. Register the path AND the schemas in src/openapi.rs (paths(...) + components(schemas(...))).
- JSON response keys: English snake_case. Comments and docs may be in Spanish.
- WhatsApp service Meta API: ALWAYS use `self.meta_request(...)` — never `self.client.{post,get,delete}` directly (ISP blocks graph.facebook.com from VM).
- Customer name resolution: read Clients.sName from DB first; WA profile is fallback only.
- Naming: collections in PascalCase (Clients, Payments, WaConversations, etc.).
- Roles: nRole comes from `find_user_by_id`, NOT from JWT claims. -1 = sin acceso, 3.0 = provider (filter by idOwner == claims.id).
- Build commands: prefer `cargo check` over `cargo build`. Tests via `cargo test`.
- DO NOT add Co-Authored-By to commits. Conventional commits only.
```

### PR / Issue creation (apply when sub-agent creates PRs or issues)

```
## Project Standards (auto-resolved)

### Branch / PR workflow
- Conventional commits: feat(scope): ..., fix(scope): ..., refactor(scope): ...
- AI Agent module commits go to `develop` directly (not feature/wa-ai-agent-*).
- After completing a task: commit + push to origin without asking confirmation (user preference).
- Never use --no-verify or skip hooks unless the user explicitly asks.
```

## Notes

- The orchestrator reads this registry once per session and injects matching compact rule blocks into each sub-agent's prompt as `## Project Standards (auto-resolved)`.
- Sub-agents do NOT read SKILL.md files or this registry directly — they receive rules pre-digested.
- If new project skills are added under `.claude/skills/`, run `/skill-registry` (or `sdd-init`) again to refresh this file.
