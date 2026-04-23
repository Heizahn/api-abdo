// Cloudflare Worker — WhatsApp Meta Relay
//
// Relayea TODAS las llamadas a Meta (graph.facebook.com + lookaside.fbsbx.com)
// para el backend api-abdo. Existe porque la VM de Debian no logra conectar
// con fiabilidad a esos hosts desde Venezuela (bloqueo/filtrado del ISP) y
// Cloudflare Workers sí tienen ruta limpia.
//
// Contrato:
//   <METHOD> https://<worker>/?url=<meta_url_url_encoded>
//   Métodos soportados: GET, POST (los que usa WhatsApp Cloud API hoy).
//   Headers obligatorios:
//     - x-relay-secret: <RELAY_SECRET>  (compartido con el backend)
//     - authorization:  Bearer <meta_access_token>  (passthrough a Meta)
//   Para POST:
//     - content-type: application/json (o lo que corresponda)
//     - body: el JSON que iría directo a Meta
//
// Seguridad:
//   - RELAY_SECRET previene que cualquier tercero use esto como proxy abierto.
//   - Whitelist de host destino (*.fbsbx.com, *.facebook.com, *.cdninstagram.com).
//
// Deploy:
//   wrangler deploy
//   wrangler secret put RELAY_SECRET  (mismo valor que el WA_MEDIA_RELAY_SECRET
//                                       del backend).

const ALLOWED_METHODS = ['GET', 'POST'];
const ALLOWED_HOST_SUFFIXES = ['.fbsbx.com', '.facebook.com', '.cdninstagram.com'];

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

    // 4. Reenviar headers relevantes a Meta
    const upstreamHeaders = new Headers();
    const auth = request.headers.get('authorization');
    if (auth) upstreamHeaders.set('authorization', auth);
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
