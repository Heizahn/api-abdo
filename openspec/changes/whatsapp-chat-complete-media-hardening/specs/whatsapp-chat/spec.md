# Delta Spec — WhatsApp Chat Complete Media Hardening

## MODIFIED Requirement: Inbound message persistence

El sistema MUST persistir un `WaMessage` para cada mensaje inbound de WhatsApp que Meta entregue en `messages[]`, excepto reacciones que MUST actualizar el mensaje objetivo y no crear una conversación nueva.

### Scenario: Known media inbound is persisted

Given Meta sends an inbound `image`, `video`, `document`, `audio`, or `sticker` message with a media id
When the webhook processes the payload
Then the backend MUST save the message with `direction = "in"`, the effective media type, `media_id`, MIME metadata when present, and a preview safe for the conversation list.

### Scenario: Unknown inbound type is not dropped

Given Meta sends an inbound message with an unknown or newly introduced type
When the webhook processes the payload
Then the backend MUST save a `WaMessage` with an effective type that can be rendered generically
And MUST preserve the raw payload for diagnostics/rendering
And MUST broadcast `MENSAJE_NUEVO` normally.

### Scenario: Non-message status does not create duplicate messages

Given Meta sends only `statuses[]` and no `messages[]`
When the webhook processes the payload
Then the backend MUST update existing messages when possible
And MUST NOT create duplicate customer messages for normal outbound delivery/read statuses.

## MODIFIED Requirement: Inbound media availability

The backend MUST treat inbound media download as eventually available when Meta provides a `media_id`, using retry/cache/fallback without blocking message persistence.

### Scenario: Media id arrives before binary is downloadable

Given an inbound media message has been saved with `media_id`
And the immediate prefetch fails due to transient network/CDN/Meta failure
When an agent later requests `GET /v1/auth-user/whatsapp/media/{media_id}`
Then the backend MUST retry the media info/body download
And SHOULD return cached bytes if any previous prefetch or request succeeded.

### Scenario: Meta reports inbound media failure without a saved message

Given Meta sends a failed status for media processing (`131052`, `131053`, or `131056`) and no matching `WaMessage` exists
When the fallback delay elapses and the message is still absent
Then the backend MUST surface the failure in the chat context instead of silently ignoring it
And SHOULD notify the customer/agent that the media did not arrive and must be resent.

## MODIFIED Requirement: Outbound rich messages

The staff chat send endpoint MUST support outbound text, template, interactive, media, location, contacts, and reaction flows already available in Meta Cloud API without regressing the 24h window and idempotency protections.

### Scenario: Freeform rich messages respect 24h window

Given a conversation has no active 24h window
When staff sends image, video, document, audio, sticker, location, contacts, interactive, or text
Then the backend MUST reject the request with the existing window-closed error semantics.

### Scenario: Template messages remain allowed outside 24h

Given a conversation has no active 24h window
When staff sends an approved template
Then the backend MUST keep the existing template flow and idempotency behavior.

## MODIFIED Requirement: API documentation

The OpenAPI and project documentation MUST describe the supported WhatsApp message types, media upload/download contract, size/MIME limits, and fallback behavior for media that Meta fails to deliver.
