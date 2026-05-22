// Normaliza contadores de no-leídos en WaConversations para compatibilidad
// entre documentos legacy (`unreadCount`) y actuales (`unread_count`).
//
// Ejecutar:
//   mongosh <MONGO_URI>/<DB_NAME> scripts/migrations/2026-05-22-normalize-wa-unread-count.js
//
// Qué hace:
// 1) Define `unread_count` como entero seguro tomando:
//    - unread_count si existe
//    - fallback unreadCount
//    - fallback 0
// 2) Escribe espejo `unreadCount` con el mismo valor para transición.
// 3) Convierte status legacy `sStatus` -> `status` si `status` no existe.

print("== Normalizando unread_count en WaConversations ==");

const col = db.getCollection("WaConversations");

const res = col.updateMany(
  {},
  [
    {
      $set: {
        unread_count: {
          $convert: {
            input: { $ifNull: ["$unread_count", "$unreadCount"] },
            to: "int",
            onError: 0,
            onNull: 0,
          },
        },
      },
    },
    {
      $set: {
        unreadCount: "$unread_count",
        status: {
          $cond: [
            { $or: [{ $eq: [{ $type: "$status" }, "missing"] }, { $eq: ["$status", null] }] },
            { $ifNull: ["$sStatus", "pending"] },
            "$status",
          ],
        },
      },
    },
  ]
);

print("Matched:", res.matchedCount, "Modified:", res.modifiedCount);
print("== OK ==");
