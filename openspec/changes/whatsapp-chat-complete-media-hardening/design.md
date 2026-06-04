# Design — whatsapp-chat-complete-media-hardening

## Architecture approach

Pattern: harden the existing WhatsApp module without changing route topology.

Current flow remains:

```text
Meta webhook
  ├─ statuses[] → update status / media-failure fallback
  └─ messages[] → infer effective type → save WaMessage → touch conversation → WS → optional AI dispatch

Agent media view
  GET /v1/auth-user/whatsapp/media/{media_id}
    → Redis cache hit OR wait for prefetch OR download from Meta/relay with retry
```

## Decisions

### 1. Persist first, download later

Inbound media MUST be saved as soon as Meta gives `media_id`. Download/prefetch is an optimization, not a requirement for the message to exist.

Rationale: if the CDN/relay is slow, the chat still shows a media bubble and the proxy can retry later.

### 2. Unknown types become generic messages with raw payload

Unknown Meta types SHOULD NOT be dropped. Store `raw_payload` and a generic preview.

Rationale: Meta evolves. A support inbox must degrade gracefully.

### 3. Media failure without a message becomes visible

When Meta reports media failure and no DB message exists, the backend should create/emit a visible placeholder or send a fallback notice. Silent ignore is unacceptable because operators interpret it as “the customer never sent the file”.

Rationale: this is an operational incident, not routine telemetry.

### 4. Keep auth group unchanged

No new public route. Media upload/download remain under staff JWT routes. Webhook remains unauthenticated but signature-verified when `WHATSAPP_APP_SECRET` is configured.

## Sequence — inbound media normal path

```text
Meta → POST /v1/webhook/whatsapp
  → infer type=image
  → save WaMessage(media_id, mime, filename)
  → spawn prefetch_media
  → broadcast MENSAJE_NUEVO
Agent opens media
  → GET /media/{media_id}
  → Redis HIT or Meta retry download
```

## Sequence — Meta media failure path

```text
Meta → POST /v1/webhook/whatsapp statuses[failed, code=131052]
  → update_message_status returns None
  → schedule fallback with delayed rechecks
  → if message still absent: surface visible failure / resend notice
```

## Compatibility

- Existing `MessageItem` fields remain.
- Existing outbound send modes remain.
- No collection rename or destructive migration.
