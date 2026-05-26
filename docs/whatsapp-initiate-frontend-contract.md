# Frontend Contract: Iniciar Conversacion WhatsApp

Endpoint: `POST /v1/auth-user/whatsapp/conversations/initiate`

## Objetivo
Definir un contrato estable de errores para que frontend muestre mensajes correctos sin adivinar causas.

## Respuesta exitosa
- HTTP `200`
- Body:
```json
{
  "ok": true,
  "data": {}
}
```

## Errores por caso de uso

| HTTP | `error` / `code` | Caso | Accion sugerida en frontend |
|---|---|---|---|
| 401 | `unauthorized` | Token invalido/expirado o no enviado | Forzar login/refresh del usuario |
| 403 | `whatsapp_chat_permission_required` | Usuario sin permiso de chat (`can_chat=false` y no superadmin) | Mostrar "No tienes permisos para usar mensajeria" |
| 400 | `whatsapp_workspace_id_invalid` | `business_phone_id` no es ObjectId valido | Corregir valor enviado desde selector de workspace |
| 404 | `whatsapp_workspace_not_found` | Workspace no existe para ese `business_phone_id` | Refrescar catalogo de workspaces y reintentar |
| 403 | `whatsapp_workspace_membership_required` | Usuario no pertenece a `workspace.agents` | Mostrar "No tienes permiso sobre este workspace" |
| 400 | `whatsapp_workspace_inactive` | Workspace desactivado | Mostrar aviso de workspace inactivo |
| 400 | `whatsapp_workspace_credentials_missing` | Falta `phone_number_id` o `access_token` en workspace | Mostrar error de configuracion y bloquear envio |
| 400 | `whatsapp_idempotency_key_required` | `idempotency_key` vacio | Generar/enviar idempotency key antes de reintentar |
| 400 | `whatsapp_recipient_invalid` | `to` invalido tras normalizacion E.164 | Validar telefono y pedir correccion |
| 400 | `missing_template_params` | Plantilla sin campos requeridos | Validar componentes/params de plantilla |

## Notas
- Para errores de dominio, el backend devuelve shape:
```json
{
  "ok": false,
  "error": "stable_code",
  "code": "stable_code",
  "message": "mensaje legible",
  "field": "campo_opcional"
}
```
- `superadmin` (`role == 0.0`) puede iniciar conversacion aunque `can_chat=false`.
