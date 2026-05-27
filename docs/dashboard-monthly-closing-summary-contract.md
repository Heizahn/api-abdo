# Dashboard Monthly Closing Summary Contract

## Endpoint

- `GET /v1/auth-user/dashboard/monthly-closing/summary`

## Query Params

- `from_date` (required, `YYYY-MM-DD`)
- `to_date` (required, `YYYY-MM-DD`)
- `owner` (optional, provider id; permission-checked by backend)

## Response

```json
{
  "total_collected_usd": 1234.56,
  "total_paid_usd": 150.00,
  "total_paid_bs": 45000.25,
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

## Validation Rules

- `from_date` and `to_date` must be valid `YYYY-MM-DD`.
- `from_date <= to_date`.
- `to_date` cannot be in the future (Caracas date).

## Notes for Frontend

- Empty range or no matching records returns zeros (not an error).
- Recommended KPI mapping:
  - `Recaudado total (USD)` -> `total_collected_usd`
  - `Pagos USD` -> `total_paid_usd`
  - `Pagos Bs` -> `total_paid_bs`
