# ork operations runbooks

Operator-facing how-tos that complement the architectural decisions in
[`docs/adrs/`](../adrs/). ADRs answer **why** the system is shaped the way it
is; pages here answer **how to run it on a Tuesday afternoon**.

| Page | Topic | Companion ADR |
| ---- | ----- | ------------- |
| [push-notifications.md](push-notifications.md) | Push delivery worker, ES256 signing, JWKS, KEK / key rotation, dead-letter triage. | [0009](../adrs/0009-push-notifications.md) |

## Adding a new runbook

1. Create `docs/operations/<topic>.md`. Aim for the same shape as
   `push-notifications.md`: a short "what this subsystem does" header, a
   config table, a "background tasks" table, troubleshooting cheat-sheet, and
   a "see also" linking to the ADR(s) that motivated the work.
2. Append a row to the table above.
3. If the runbook covers a brand-new ADR, link the runbook from the ADR's
   `Affected ork modules` section so readers can hop between the two.
