use crate::store::{map_sqlite_err, now_unix_ms, SqliteLedgerStore};
use async_trait::async_trait;
use core_contracts::{BudgetLedger, BudgetReservation, ContractError, MicroUsd};
use rusqlite::{params, TransactionBehavior};
use uuid::Uuid;

#[async_trait]
impl BudgetLedger for SqliteLedgerStore {
    async fn reserve(
        &self,
        run_id: &str,
        node_id: &str,
        estimated_micro_usd: MicroUsd,
        run_limit_micro_usd: MicroUsd,
    ) -> Result<BudgetReservation, ContractError> {
        if estimated_micro_usd < 0 || run_limit_micro_usd < 0 {
            return Err(ContractError::PolicyViolation(
                "negative budget or estimate is forbidden".into(),
            ));
        }

        let run_id = run_id.to_string();
        let node_id = node_id.to_string();

        self.with_conn(move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(map_sqlite_err)?;

            let settled: MicroUsd = tx
                .query_row(
                    "SELECT COALESCE(SUM(cost_micro_usd), 0) FROM ledger_events WHERE run_id = ?1",
                    params![&run_id],
                    |row| row.get(0),
                )
                .map_err(map_sqlite_err)?;

            let pending: MicroUsd = tx
                .query_row(
                    "SELECT COALESCE(SUM(reserved_micro_usd), 0)
                     FROM budget_reservations
                     WHERE run_id = ?1 AND status = 'pending'",
                    params![&run_id],
                    |row| row.get(0),
                )
                .map_err(map_sqlite_err)?;

            let projected = settled
                .saturating_add(pending)
                .saturating_add(estimated_micro_usd);

            if projected > run_limit_micro_usd {
                return Err(ContractError::BudgetExceeded {
                    projected,
                    limit: run_limit_micro_usd,
                });
            }

            let reservation_id = Uuid::new_v4().to_string();
            let now = now_unix_ms();

            tx.execute(
                "INSERT INTO budget_reservations
                 (reservation_id, run_id, node_id, reserved_micro_usd, status, created_at_ms, updated_at_ms)
                 VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?5)",
                params![&reservation_id, &run_id, &node_id, estimated_micro_usd, now],
            )
            .map_err(map_sqlite_err)?;

            tx.commit().map_err(map_sqlite_err)?;

            Ok(BudgetReservation {
                reservation_id,
                run_id,
                reserved_micro_usd: estimated_micro_usd,
            })
        })
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn settle(
        &self,
        reservation: &BudgetReservation,
        node_id: &str,
        model_tier: Option<&str>,
        prompt_tokens: u64,
        completion_tokens: u64,
        actual_micro_usd: MicroUsd,
        payload_json: &str,
    ) -> Result<(), ContractError> {
        let res_id = reservation.reservation_id.clone();
        let run_id = reservation.run_id.clone();
        let node_id = node_id.to_string();
        let model_tier = model_tier.map(str::to_string);
        let payload_json = payload_json.to_string();

        self.with_conn(move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(map_sqlite_err)?;

            let now = now_unix_ms();
            let updated = tx
                .execute(
                    "UPDATE budget_reservations
                     SET status = 'settled', updated_at_ms = ?1
                     WHERE reservation_id = ?2 AND status = 'pending'",
                    params![now, &res_id],
                )
                .map_err(map_sqlite_err)?;

            if updated == 0 {
                return Err(ContractError::NotFound(format!(
                    "pending budget reservation not found: {res_id}"
                )));
            }

            tx.execute(
                "INSERT INTO ledger_events
                 (run_id, node_id, event_type, model_tier, prompt_tokens, completion_tokens, cost_micro_usd, payload_json, created_at_ms)
                 VALUES (?1, ?2, 'llm_call', ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    &run_id,
                    &node_id,
                    model_tier.as_deref(),
                    prompt_tokens as i64,
                    completion_tokens as i64,
                    actual_micro_usd,
                    &payload_json,
                    now
                ],
            )
            .map_err(map_sqlite_err)?;

            tx.commit().map_err(map_sqlite_err)?;
            Ok(())
        })
        .await
    }

    async fn release(&self, reservation: &BudgetReservation) -> Result<(), ContractError> {
        let res_id = reservation.reservation_id.clone();
        self.with_conn(move |conn| {
            let now = now_unix_ms();
            conn.execute(
                "UPDATE budget_reservations
                 SET status = 'released', updated_at_ms = ?1
                 WHERE reservation_id = ?2 AND status = 'pending'",
                params![now, &res_id],
            )
            .map_err(map_sqlite_err)?;
            Ok(())
        })
        .await
    }

    async fn total_spent(&self, run_id: &str) -> Result<MicroUsd, ContractError> {
        let run_id = run_id.to_string();
        self.with_conn(move |conn| {
            let spent: MicroUsd = conn
                .query_row(
                    "SELECT COALESCE(SUM(cost_micro_usd), 0) FROM ledger_events WHERE run_id = ?1",
                    params![&run_id],
                    |row| row.get(0),
                )
                .map_err(map_sqlite_err)?;
            Ok(spent)
        })
        .await
    }
}
