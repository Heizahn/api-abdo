# Security Patch — Auth + WebSocket

## Resumen de contrato (backend)

1. Login/refresh ahora emiten cookies HttpOnly:
- `abdo_client_at` / `abdo_client_rt`
- `abdo_staff_at` / `abdo_staff_rt`

2. Flags de seguridad de cookie:
- `HttpOnly` siempre.
- `Secure` según `AUTH_COOKIE_SECURE` (default `true`).
- `SameSite` según `AUTH_COOKIE_SAME_SITE` (default `Lax`).
- `Domain` opcional vía `AUTH_COOKIE_DOMAIN`.

3. Refresh token:
- Canal primario: cookie refresh.
- Rotación en cada refresh.
- Detección de reuso vía Redis por `family + jti`.
- Códigos para frontend:
  - `session_expired`
  - `invalid_refresh_token`
  - `refresh_token_reused`

4. WebSocket:
- Canal primario: cookie de sesión staff (`abdo_staff_at`) en handshake.
- `?token=` sólo en compat temporal.

5. `owner` query en dashboard/clients:
- Si el caller no tiene permiso para el owner solicitado => `403`.
- Ya no se “ignora silenciosamente” el owner inválido.

6. Error funcional de cambio de contraseña:
- `wrong_password` ahora responde `403` (no `401`).

## Variables nuevas

- `FRONTEND_ORIGINS` (CSV, ej: `https://front.app,https://staging.front.app`)
- `CORS_ALLOW_CREDENTIALS` (`true|false`, default `true`)
- `AUTH_COOKIE_SECURE` (`true|false`, default `true`)
- `AUTH_COOKIE_SAME_SITE` (`Lax|Strict|None`, default `Lax`)
- `AUTH_COOKIE_DOMAIN` (opcional)
- `AUTH_COMPAT_ALLOW_BEARER` (`true|false`, default `true`)
- `AUTH_COMPAT_ALLOW_REFRESH_BODY` (`true|false`, default `true`)
- `AUTH_COMPAT_ALLOW_WS_QUERY` (`true|false`, default `true`)
- `AUTH_COMPAT_UNTIL` (`YYYY-MM-DD`, opcional)

## Ventana de compatibilidad sugerida (1 sprint)

Ejemplo:
- Fecha actual: **2026-05-22**
- Corte sugerido: **2026-06-12**

Config:

```env
AUTH_COMPAT_UNTIL=2026-06-12
AUTH_COMPAT_ALLOW_BEARER=true
AUTH_COMPAT_ALLOW_REFRESH_BODY=true
AUTH_COMPAT_ALLOW_WS_QUERY=true
```

Al pasar la fecha, el backend desactiva los canales legacy automáticamente.
