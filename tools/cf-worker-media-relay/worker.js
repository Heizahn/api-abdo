// Cloudflare Worker — WhatsApp Media Relay
//
// Relayea descargas de media de Meta (lookaside.fbsbx.com) para el backend
// api-abdo. Existe porque la VM de Debian no logra conectar a esa CDN desde
// Venezuela y Cloudflare Workers sí tienen ruta limpia.
//
// Contrato:
//   GET https://<worker>/?url=<signed_meta_url_url_encoded>
//   Headers obligatorios:
//     - x-relay-secret: <RELAY_SECRET>   (compartido con el backend)
//     - authorization:  Bearer <meta_access_token>  (tal cual se lo
//       mandaríamos a Meta; lo pasamos transparente)
//
// Seguridad:
//   - RELAY_SECRET previene que cualquier tercero use esto de proxy abierto
//     para salir a internet por Cloudflare.
//   - Se valida que el host destino sea *.fbsbx.com o *.facebook.com.
//     Sin esto, quien sepa el secret podría usar el worker para cualquier
//     cosa.
//
// Deploy:
//   wrangler deploy  (ver wrangler.toml al lado)
//   wrangler secret put RELAY_SECRET
//   # el mismo valor va en el .env del backend como WA_MEDIA_RELAY_SECRET

export default {
  async fetch(request, env) {
    if (request.method !== 'GET') {
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

    // 3. Solo permitimos hosts de Meta — evita usar esto como open proxy
    const host = targetUrl.hostname.toLowerCase();
    const allowedSuffixes = ['.fbsbx.com', '.facebook.com', '.cdninstagram.com'];
    const hostAllowed = allowedSuffixes.some((s) => host.endsWith(s));
    if (!hostAllowed) {
      return new Response('host not allowed', { status: 403 });
    }

    // 4. Fetch transparente — pasamos Authorization + Accept tal cual
    const upstreamHeaders = new Headers();
    const auth = request.headers.get('authorization');
    if (auth) upstreamHeaders.set('authorization', auth);
    upstreamHeaders.set('accept', '*/*');

    let upstream;
    try {
      upstream = await fetch(target, {
        method: 'GET',
        headers: upstreamHeaders,
        redirect: 'follow',
        // Cloudflare cachea por URL; estos binarios son inmutables por 5 min
        // (TTL de la URL firmada de Meta), así que habilitamos cache a 300s.
        cf: { cacheEverything: true, cacheTtl: 300 },
      });
    } catch (err) {
      return new Response(`upstream fetch failed: ${err.message}`, { status: 502 });
    }

    // 5. Pass-through de headers útiles + body streaming
    const outHeaders = new Headers();
    const ct = upstream.headers.get('content-type');
    if (ct) outHeaders.set('content-type', ct);
    const cl = upstream.headers.get('content-length');
    if (cl) outHeaders.set('content-length', cl);
    const etag = upstream.headers.get('etag');
    if (etag) outHeaders.set('etag', etag);

    return new Response(upstream.body, {
      status: upstream.status,
      statusText: upstream.statusText,
      headers: outHeaders,
    });
  },
};
