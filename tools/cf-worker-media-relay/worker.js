// Cloudflare Worker — Outbound Relay (WhatsApp Meta + Gemini)
//
// Relayea las llamadas externas del backend api-abdo:
//   - Meta WhatsApp Cloud API (graph.facebook.com + lookaside.fbsbx.com).
//   - Gemini Generative Language API (generativelanguage.googleapis.com)
//     para el módulo AI Agent.
//
// Existe porque la VM de Debian no logra conectar con fiabilidad a esos
// hosts desde Venezuela (bloqueo/filtrado del ISP) y Cloudflare Workers
// sí tienen ruta limpia.
//
// Contrato:
//   <METHOD> https://<worker>/?url=<destination_url_url_encoded>
//   Métodos soportados: GET, POST.
//   Headers obligatorios:
//     - x-relay-secret: <RELAY_SECRET>  (compartido con el backend)
//   Headers passthrough (cuando aplica):
//     - authorization:    Bearer <token>          (Meta WA Cloud API)
//     - x-goog-api-key:   <gemini_api_key>        (Gemini)
//     - content-type:     application/json | ...  (POST body type)
//   Para POST:
//     - body: el JSON que iría directo al destino
//
// Seguridad:
//   - RELAY_SECRET previene que cualquier tercero use esto como proxy abierto.
//   - Whitelist de host destino (Meta + Google AI). Cualquier intento de
//     usar el relay para otro host devuelve 403.
//
// Deploy:
//   wrangler deploy
//   wrangler secret put RELAY_SECRET  (compartido con el backend; la misma
//                                       pieza puede atender WA y AI o se
//                                       puede correr en workers separados).

const ALLOWED_METHODS = ['GET', 'POST'];
const ALLOWED_HOST_SUFFIXES = [
  // WhatsApp / Meta
  '.fbsbx.com',
  '.facebook.com',
  '.cdninstagram.com',
  // Gemini (Google Generative Language API)
  '.googleapis.com',
];

export default {
  async fetch(request, env) {
    if (!ALLOWED_METHODS.includes(request.method)) {
      return new Response('method not allowed', { status: 405 });
    }

    // 1. Validar secret compartido
    const presented = request.headers.get('x-relay-secret');
    const expected = env.RELAY_SECRET;
    if (!expected || !presented || presented !== expected) {
      return new Response('forbidden', { status: 403 });
    }

    // 2. Extraer URL destino
    const url = new URL(request.url);
    const target = url.searchParams.get('url');
    if (!target) {
      return new Response('missing url param', { status: 400 });
    }

    let targetUrl;
    try {
      targetUrl = new URL(target);
    } catch {
      return new Response('invalid url', { status: 400 });
    }

    // 3. Validar host contra whitelist
    const host = targetUrl.hostname.toLowerCase();
    const hostAllowed = ALLOWED_HOST_SUFFIXES.some((s) => host.endsWith(s));
    if (!hostAllowed) {
      return new Response('host not allowed', { status: 403 });
    }

    // 4. Reenviar headers relevantes al destino
    //    - authorization: Meta usa Bearer.
    //    - x-goog-api-key: Gemini lo prefiere así (más seguro que ?key=...
    //      en query string — no termina en logs ni access_log).
    const upstreamHeaders = new Headers();
    const auth = request.headers.get('authorization');
    if (auth) upstreamHeaders.set('authorization', auth);
    const googKey = request.headers.get('x-goog-api-key');
    if (googKey) upstreamHeaders.set('x-goog-api-key', googKey);
    const ct = request.headers.get('content-type');
    if (ct) upstreamHeaders.set('content-type', ct);
    upstreamHeaders.set('accept', '*/*');

    // 5. Fetch upstream. GET puede aprovechar la caché de Cloudflare
    // (URLs firmadas de media caducan a ~5 min). POST nunca se cachea
    // (Meta devuelve wa_message_id único por request).
    const init = {
      method: request.method,
      headers: upstreamHeaders,
      redirect: 'follow',
    };
    if (request.method === 'GET') {
      init.cf = { cacheEverything: true, cacheTtl: 300 };
    } else {
      init.body = request.body;
    }

    let upstream;
    try {
      upstream = await fetch(target, init);
    } catch (err) {
      return new Response(`upstream fetch failed: ${err.message}`, { status: 502 });
    }

    // 6. Pass-through de response: status + headers útiles + body streaming.
    const outHeaders = new Headers();
    for (const h of ['content-type', 'content-length', 'etag']) {
      const v = upstream.headers.get(h);
      if (v) outHeaders.set(h, v);
    }
    return new Response(upstream.body, {
      status: upstream.status,
      statusText: upstream.statusText,
      headers: outHeaders,
    });
  },
};
