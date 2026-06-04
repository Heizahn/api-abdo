# Proposal — whatsapp-chat-complete-media-hardening

## Intent

El chat de WhatsApp debe comportarse como un inbox confiable: aceptar y persistir todos los tipos de mensajes que Meta puede entregar, mostrar media inbound sin depender de un único intento de descarga, y avisar de forma explícita cuando Meta informa que un archivo del cliente no llegó al backend.

El objetivo NO es reescribir el módulo ni romper contratos existentes. El cambio fortalece el flujo actual (`webhook → WaMessages → media proxy/cache → WebSocket`) con defensas incrementales.

## Problem

El backend ya soporta varios tipos (`text`, media, `location`, `contacts`, `interactive`, `button`, `reaction`, `order`, `system`, `referral`, `unsupported`) y hace prefetch de media. Sin embargo, hay huecos operativos:

- Si Meta envía un tipo nuevo o poco modelado, el front necesita un mensaje persistido con `raw_payload` y un preview legible, no un silencio.
- Si una imagen/video/documento/audio/sticker inbound llega con `media_id` pero la descarga falla temporalmente, el sistema debe mantener el mensaje y permitir reintento/caché posterior.
- Si Meta reporta un fallo de media inbound sin que exista documento en DB, el sistema debe crear/avisar un placeholder visible para que el agente NO piense que “no llegó nada”.
- La documentación debe dejar claro el contrato de tipos, media download/upload, límites y fallback.

## Scope

### In scope

- Fortalecer inferencia/persistencia de tipos inbound para cubrir tipos conocidos y desconocidos.
- Mejorar placeholders y `raw_payload` para mensajes no estándar.
- Endurecer fallback de media inbound fallida reportada por Meta.
- Mantener rutas existentes y compatibilidad con `MessageItem` actual.
- Actualizar OpenAPI y documentación del proyecto sobre contratos de chat/media.
- Verificar con `cargo check` y pruebas relevantes disponibles.

### Out of scope

- Cambios de frontend.
- Migraciones destructivas de MongoDB.
- Reemplazar el WebSocket o la arquitectura del módulo.
- Garantizar que Meta nunca falle: cuando Meta no entrega el binario, el backend sólo puede detectar, registrar, notificar y permitir reintento/placeholder. NO existe magia para descargar un archivo que Meta nunca procesó.

## Affected modules

- `src/modules/whatsapp/handler.rs`
- `src/modules/whatsapp/service.rs`
- `src/models/whatsapp.rs`
- `src/openapi.rs`
- `openspec/specs/whatsapp-chat/`
- `AGENTS.md` / documentación operativa si aplica

## Rollback plan

El cambio será incremental. Si falla en producción:

1. Revertir el commit completo.
2. Las colecciones existentes (`WaMessages`, `WaConversations`) siguen siendo compatibles porque no se eliminan campos ni se cambia la clave de mensajes.
3. Los nuevos placeholders o campos opcionales permanecen como documentos válidos; el front puede ignorarlos si no los soporta aún.
