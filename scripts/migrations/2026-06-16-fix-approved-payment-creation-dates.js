// Repara Payments auto-creados al aprobar PaymentReports.
//
// Contexto:
// - Antes, Payments.dCreation podía tomar PaymentReports.dPaymentDate.
// - Si dPaymentDate venía solo con fecha, quedaba con hora por defecto.
// - Para pagos creados al aprobar un reporte, dCreation debe ser la hora real
//   de aprobación, guardada en PaymentReports.dEdition.
//
// Seguridad:
// - Solo toca Payments activos/no anulados.
// - Solo toca Payments con idPaymentReport.
// - Solo toca Payments auto-creados por aprobación: sCommentary empieza con
//   "Reporte aprobado.". Esto evita sobrescribir pagos que ya existían y solo
//   fueron vinculados al reporte.
// - Solo usa PaymentReports en sState="Verificado" con dEdition válido.
//
// Ejecutar con:
//   mongosh <MONGO_URI> scripts/migrations/2026-06-16-fix-approved-payment-creation-dates.js

print("=".repeat(70));
print("🛠️  Reparando dCreation de Payments creados desde PaymentReports aprobados");
print("=".repeat(70));
print("📍 Base de datos: " + db.getName());
print("");

function toMillis(value) {
  if (!value) return null;

  if (value instanceof Date) {
    const ms = value.getTime();
    return Number.isNaN(ms) ? null : ms;
  }

  if (typeof value === "string") {
    const trimmed = value.trim();
    if (!trimmed) return null;

    const parsed = new Date(trimmed);
    const ms = parsed.getTime();
    return Number.isNaN(ms) ? null : ms;
  }

  return null;
}

let scanned = 0;
let updated = 0;
let skippedNoReport = 0;
let skippedNoEdition = 0;
let skippedAlreadyOk = 0;

const cursor = db.Payments.find(
  {
    idPaymentReport: { $exists: true, $ne: null },
    sState: { $ne: "Anulado" },
    sCommentary: /^Reporte aprobado\./,
  },
  {
    _id: 1,
    idPaymentReport: 1,
    dCreation: 1,
  },
);

cursor.forEach((payment) => {
  scanned += 1;

  const report = db.PaymentReports.findOne(
    {
      _id: payment.idPaymentReport,
      sState: "Verificado",
      dEdition: { $exists: true, $ne: null },
    },
    {
      _id: 1,
      dEdition: 1,
    },
  );

  if (!report) {
    skippedNoReport += 1;
    return;
  }

  const reportEditionMs = toMillis(report.dEdition);
  if (reportEditionMs === null) {
    skippedNoEdition += 1;
    return;
  }

  const paymentCreationMs = toMillis(payment.dCreation);
  if (paymentCreationMs === reportEditionMs) {
    skippedAlreadyOk += 1;
    return;
  }

  db.Payments.updateOne(
    { _id: payment._id, sState: { $ne: "Anulado" } },
    { $set: { dCreation: report.dEdition } },
  );
  updated += 1;
});

print("📊 Resultado:");
print("  Revisados: " + scanned);
print("  Actualizados: " + updated);
print("  Saltados sin reporte verificado/dEdition: " + skippedNoReport);
print("  Saltados por dEdition inválido: " + skippedNoEdition);
print("  Saltados porque ya estaban correctos: " + skippedAlreadyOk);
print("");
print("✅ Reparación finalizada");
