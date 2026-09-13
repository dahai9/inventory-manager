//! Record an operator's confirmation of an existing test result inside the
//! receipt/return transaction. No second scan or separately committed command.
use sqlx::{Postgres, Sqlite, Transaction};
use uuid::Uuid;

pub(crate) struct Confirmation<'a> {
    pub source: &'a str,
    pub source_id: &'a str,
    pub actor: &'a str,
    pub at: &'a str,
    pub request_id: &'a str,
    pub notes: Option<&'a str>,
}

impl Confirmation<'_> {
    fn kind(&self) -> &'static str {
        if self.source == "receipt_prechecked" {
            "initial"
        } else {
            "retest"
        }
    }
    fn notes(&self) -> String {
        let label = if self.source == "receipt_prechecked" {
            "入库已质检确认"
        } else {
            "客户确认合格，退货直接放行"
        };
        format!(
            "{label}{}",
            self.notes.map(|n| format!("：{n}")).unwrap_or_default()
        )
    }
}

pub(crate) async fn record_sqlite(
    tx: &mut Transaction<'_, Sqlite>,
    workspace: &str,
    units: &[String],
    confirmation: Confirmation<'_>,
) -> Result<(), sqlx::Error> {
    let id = Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO quality_inspections (id, workspace_id, inspection_no, inspection_type, status, inspector_id, inspected_at, idempotency_key, request_id, created_at) VALUES (?1, ?2, ?3, ?4, 'completed', ?5, ?6, ?7, ?8, ?6)")
        .bind(&id).bind(workspace).bind(format!("QR-{id}")).bind(confirmation.kind()).bind(confirmation.actor).bind(confirmation.at).bind(format!("confirmation:{id}")).bind(confirmation.request_id).execute(&mut **tx).await?;
    let measurements = serde_json::json!({"confirmation_source": confirmation.source, "source_id": confirmation.source_id}).to_string();
    for unit in units {
        sqlx::query("INSERT INTO quality_inspection_results (id, workspace_id, inspection_id, inventory_unit_id, result, measurements_json, notes, created_at) VALUES (?1, ?2, ?3, ?4, 'passed', ?5, ?6, ?7)")
            .bind(Uuid::now_v7().to_string()).bind(workspace).bind(&id).bind(unit).bind(&measurements).bind(confirmation.notes()).bind(confirmation.at).execute(&mut **tx).await?;
    }
    Ok(())
}

pub(crate) async fn record_postgres(
    tx: &mut Transaction<'_, Postgres>,
    tenant: Uuid,
    units: &[Uuid],
    confirmation: Confirmation<'_>,
    actor: Uuid,
) -> Result<(), sqlx::Error> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO quality_inspections (tenant_id, id, inspection_no, inspection_type, status, inspector_id, inspected_at, idempotency_key, request_id) VALUES ($1, $2, $3, $4, 'completed', $5, $6::timestamptz, $7, $8)")
        .bind(tenant).bind(id).bind(format!("QR-{id}")).bind(confirmation.kind()).bind(actor).bind(confirmation.at).bind(format!("confirmation:{id}")).bind(confirmation.request_id).execute(&mut **tx).await?;
    let measurements = serde_json::json!({"confirmation_source": confirmation.source, "source_id": confirmation.source_id}).to_string();
    for unit in units {
        sqlx::query("INSERT INTO quality_inspection_results (tenant_id, id, inspection_id, inventory_unit_id, result, measurements_json, notes) VALUES ($1, $2, $3, $4, 'passed', $5::jsonb, $6)")
            .bind(tenant).bind(Uuid::now_v7()).bind(id).bind(unit).bind(&measurements).bind(confirmation.notes()).execute(&mut **tx).await?;
    }
    Ok(())
}
