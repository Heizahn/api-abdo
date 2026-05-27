# Dashboard Monthly Closing Summary Contract

## Endpoint

- `GET /v1/auth-user/dashboard/monthly-closing/summary`

## Query Params

- `month` (optional, `YYYY-MM`)
- `from_date` (optional, `YYYY-MM-DD`)
- `to_date` (optional, `YYYY-MM-DD`)
- `owner` (optional, provider id; permission-checked by backend)

Rules:
- Backward compatibility:
  - If `from_date`/`to_date` are present, backend uses those dates (they have priority).
  - If only one date is sent, backend treats it as a one-day range (`from_date == to_date`).
  - If no dates are sent, backend falls back to `month`.

## Response

```json
{
  "total_collected_usd": 1234.56,
  "total_paid_usd": 150.00,
  "total_paid_bs": 45000.25,
  "total_pending_usd": 320.10,
  "currency_meta": {
    "usd_decimals": 2,
    "bs_decimals": 2,
    "timezone": "America/Caracas"
  }
}
```

## Business Rules

- Timezone for date range cut-off: `America/Caracas`.
- Included records: only `Payments.sState == "Activo"` (approved/confirmed path in current domain).
- `total_collected_usd`: sum of `Payments.nAmount` in date range.
- `total_paid_usd`: sum of `Payments.nAmount` where `Payments.bUSD == true`.
- `total_paid_bs`: sum of `Payments.nBs` in date range.
- `total_pending_usd`: aligned with current monthly-closing pending logic:
  - Source: active clients negative balances (`Clients.nBalance < 0`) filtered by owner.
  - Formula: `SUM(ABS(nBalance))`.
  - Applies only when the selected range belongs to the current month (`America/Caracas`).
  - For historical months/ranges, returns `0.0`.

## Validation Rules

- `from_date` and `to_date` must be valid `YYYY-MM-DD`.
- `from_date <= to_date`.
- `month` must be valid `YYYY-MM` and cannot be a future month.

## Notes for Frontend

- Empty range or no matching records returns zeros (not an error).
- Recommended KPI mapping:
  - `Recaudado total (USD)` -> `total_collected_usd`
  - `Pagos USD` -> `total_paid_usd`
  - `Pagos Bs` -> `total_paid_bs`
  - `Pendiente (USD)` -> `total_pending_usd`
