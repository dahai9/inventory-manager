use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DomainError {
    EmptyField(&'static str),
    InvalidQuantity {
        field: &'static str,
        value: u32,
    },
    InvalidTransition {
        aggregate: &'static str,
        from: String,
        to: String,
    },
    MismatchedReference {
        field: &'static str,
        expected: String,
        actual: String,
    },
    QualityNotEligible(QualityStatus),
    InventoryNotAvailable(InventoryStatus),
    OrderLineFullyAllocated,
}

impl fmt::Display for DomainError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyField(field) => write!(formatter, "{field} must not be empty"),
            Self::InvalidQuantity { field, value } => {
                write!(formatter, "{field} must be greater than zero, got {value}")
            }
            Self::InvalidTransition {
                aggregate,
                from,
                to,
            } => write!(
                formatter,
                "invalid {aggregate} transition from {from} to {to}"
            ),
            Self::MismatchedReference {
                field,
                expected,
                actual,
            } => write!(
                formatter,
                "{field} does not match: expected {expected}, got {actual}"
            ),
            Self::QualityNotEligible(status) => {
                write!(formatter, "quality status {status} is not eligible")
            }
            Self::InventoryNotAvailable(status) => {
                write!(formatter, "inventory status {status} is not available")
            }
            Self::OrderLineFullyAllocated => write!(formatter, "order line is fully allocated"),
        }
    }
}

impl Error for DomainError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentStatus {
    Draft,
    Posted,
    Voided,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InventoryStatus {
    Received,
    Available,
    Reserved,
    Shipped,
    Delivered,
    Quarantined,
    Scrapped,
    ReturnedToOwner,
    Voided,
}

impl fmt::Display for InventoryStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Received => "received",
            Self::Available => "available",
            Self::Reserved => "reserved",
            Self::Shipped => "shipped",
            Self::Delivered => "delivered",
            Self::Quarantined => "quarantined",
            Self::Scrapped => "scrapped",
            Self::ReturnedToOwner => "returned_to_owner",
            Self::Voided => "voided",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityStatus {
    Untested,
    Testing,
    Passed,
    Failed,
    Waived,
}

impl fmt::Display for QualityStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Untested => "untested",
            Self::Testing => "testing",
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Waived => "waived",
        })
    }
}

impl QualityStatus {
    pub fn allows_allocation(self) -> bool {
        matches!(self, Self::Passed | Self::Waived)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InspectionKind {
    Initial,
    Retest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InspectionStatus {
    InProgress,
    Completed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityOutcome {
    Passed,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AllocationStatus {
    Active,
    Released,
    Shipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShipmentLineStatus {
    Shipped,
    Delivered,
    Returned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryOutcome {
    Accepted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReturnDisposition {
    #[serde(rename = "quarantine")]
    Quarantined,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboundReceipt {
    pub id: String,
    pub owner_party_id: String,
    pub warehouse_id: String,
    pub received_at: String,
    pub created_by: String,
    pub status: DocumentStatus,
}

impl InboundReceipt {
    pub fn new(
        id: impl Into<String>,
        owner_party_id: impl Into<String>,
        warehouse_id: impl Into<String>,
        received_at: impl Into<String>,
        created_by: impl Into<String>,
    ) -> Result<Self, DomainError> {
        Ok(Self {
            id: required("inbound_receipt.id", id)?,
            owner_party_id: required("inbound_receipt.owner_party_id", owner_party_id)?,
            warehouse_id: required("inbound_receipt.warehouse_id", warehouse_id)?,
            received_at: required("inbound_receipt.received_at", received_at)?,
            created_by: required("inbound_receipt.created_by", created_by)?,
            status: DocumentStatus::Draft,
        })
    }

    pub fn post(&mut self) -> Result<(), DomainError> {
        if self.status != DocumentStatus::Draft {
            return Err(invalid_transition(
                "inbound receipt",
                document_status_name(self.status),
                "posted",
            ));
        }
        self.status = DocumentStatus::Posted;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboundReceiptLine {
    pub id: String,
    pub inbound_receipt_id: String,
    pub sku_id: String,
    pub declared_quantity: u32,
}

impl InboundReceiptLine {
    pub fn new(
        id: impl Into<String>,
        inbound_receipt_id: impl Into<String>,
        sku_id: impl Into<String>,
        declared_quantity: u32,
    ) -> Result<Self, DomainError> {
        if declared_quantity == 0 {
            return Err(DomainError::InvalidQuantity {
                field: "inbound_receipt_line.declared_quantity",
                value: declared_quantity,
            });
        }

        Ok(Self {
            id: required("inbound_receipt_line.id", id)?,
            inbound_receipt_id: required(
                "inbound_receipt_line.inbound_receipt_id",
                inbound_receipt_id,
            )?,
            sku_id: required("inbound_receipt_line.sku_id", sku_id)?,
            declared_quantity,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewInventoryUnit {
    pub id: String,
    pub barcode: String,
    pub inbound_receipt_line_id: String,
    pub owner_party_id: String,
    pub sku_id: String,
    pub location_id: String,
    pub received_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryUnit {
    pub id: String,
    pub barcode: String,
    pub inbound_receipt_line_id: String,
    pub owner_party_id: String,
    pub sku_id: String,
    pub location_id: String,
    pub received_at: String,
    pub inventory_status: InventoryStatus,
    pub quality_status: QualityStatus,
    pub active_allocation_id: Option<String>,
    pub latest_shipment_line_id: Option<String>,
    pub version: u64,
}

impl InventoryUnit {
    pub fn receive(input: NewInventoryUnit) -> Result<Self, DomainError> {
        Ok(Self {
            id: required("inventory_unit.id", input.id)?,
            barcode: required("inventory_unit.barcode", input.barcode)?,
            inbound_receipt_line_id: required(
                "inventory_unit.inbound_receipt_line_id",
                input.inbound_receipt_line_id,
            )?,
            owner_party_id: required("inventory_unit.owner_party_id", input.owner_party_id)?,
            sku_id: required("inventory_unit.sku_id", input.sku_id)?,
            location_id: required("inventory_unit.location_id", input.location_id)?,
            received_at: required("inventory_unit.received_at", input.received_at)?,
            inventory_status: InventoryStatus::Received,
            quality_status: QualityStatus::Untested,
            active_allocation_id: None,
            latest_shipment_line_id: None,
            version: 1,
        })
    }

    pub fn begin_quality_inspection(
        &mut self,
        id: impl Into<String>,
        kind: InspectionKind,
        inspector_id: impl Into<String>,
        started_at: impl Into<String>,
    ) -> Result<QualityInspection, DomainError> {
        let id = required("quality_inspection.id", id)?;
        let inspector_id = required("quality_inspection.inspector_id", inspector_id)?;
        let started_at = required("quality_inspection.started_at", started_at)?;

        let is_valid = match kind {
            InspectionKind::Initial => {
                self.inventory_status == InventoryStatus::Received
                    && self.quality_status == QualityStatus::Untested
            }
            InspectionKind::Retest => {
                self.inventory_status == InventoryStatus::Quarantined
                    && matches!(
                        self.quality_status,
                        QualityStatus::Failed | QualityStatus::Passed | QualityStatus::Waived
                    )
            }
        };
        if !is_valid {
            return Err(invalid_transition(
                "quality",
                format!("{}+{}", self.inventory_status, self.quality_status),
                match kind {
                    InspectionKind::Initial => "initial_testing",
                    InspectionKind::Retest => "retesting",
                },
            ));
        }

        self.quality_status = QualityStatus::Testing;
        self.bump_version();
        Ok(QualityInspection {
            id,
            inventory_unit_id: self.id.clone(),
            kind,
            inspector_id,
            started_at,
            completed_at: None,
            status: InspectionStatus::InProgress,
            outcome: None,
            defect_code: None,
            notes: None,
        })
    }

    pub fn complete_quality_inspection(
        &mut self,
        inspection: &mut QualityInspection,
        outcome: QualityOutcome,
        completed_at: impl Into<String>,
        defect_code: Option<String>,
        notes: Option<String>,
    ) -> Result<(), DomainError> {
        let completed_at = required("quality_inspection.completed_at", completed_at)?;
        ensure_reference(
            "quality_inspection.inventory_unit_id",
            &self.id,
            &inspection.inventory_unit_id,
        )?;
        if inspection.status != InspectionStatus::InProgress
            || self.quality_status != QualityStatus::Testing
        {
            return Err(invalid_transition(
                "quality inspection",
                format!(
                    "{}+{}",
                    inspection_status_name(inspection.status),
                    self.quality_status
                ),
                match outcome {
                    QualityOutcome::Passed => "passed",
                    QualityOutcome::Failed => "failed",
                },
            ));
        }
        if !matches!(
            self.inventory_status,
            InventoryStatus::Received | InventoryStatus::Quarantined
        ) {
            return Err(invalid_transition(
                "inventory",
                self.inventory_status.to_string(),
                "quality_completion",
            ));
        }

        inspection.completed_at = Some(completed_at);
        inspection.status = InspectionStatus::Completed;
        inspection.outcome = Some(outcome);
        inspection.defect_code = trimmed_optional(defect_code);
        inspection.notes = trimmed_optional(notes);

        match outcome {
            QualityOutcome::Passed => {
                self.quality_status = QualityStatus::Passed;
                self.inventory_status = InventoryStatus::Available;
            }
            QualityOutcome::Failed => {
                self.quality_status = QualityStatus::Failed;
                self.inventory_status = InventoryStatus::Quarantined;
            }
        }
        self.bump_version();
        Ok(())
    }

    pub fn waive_quality(
        &mut self,
        waiver_id: impl Into<String>,
        granted_by: impl Into<String>,
        reason: impl Into<String>,
        granted_at: impl Into<String>,
    ) -> Result<QualityWaiver, DomainError> {
        let waiver_id = required("quality_waiver.id", waiver_id)?;
        let granted_by = required("quality_waiver.granted_by", granted_by)?;
        let reason = required("quality_waiver.reason", reason)?;
        let granted_at = required("quality_waiver.granted_at", granted_at)?;

        let valid_state = matches!(
            self.inventory_status,
            InventoryStatus::Received | InventoryStatus::Quarantined
        ) && matches!(
            self.quality_status,
            QualityStatus::Untested | QualityStatus::Failed
        );
        if !valid_state {
            return Err(invalid_transition(
                "quality",
                format!("{}+{}", self.inventory_status, self.quality_status),
                "waived",
            ));
        }

        let previous_status = self.quality_status;
        self.quality_status = QualityStatus::Waived;
        self.inventory_status = InventoryStatus::Available;
        self.bump_version();
        Ok(QualityWaiver {
            id: waiver_id,
            inventory_unit_id: self.id.clone(),
            previous_status,
            granted_by,
            reason,
            granted_at,
        })
    }

    pub fn ensure_allocation_eligible(&self, required_sku_id: &str) -> Result<(), DomainError> {
        ensure_reference("inventory_unit.sku_id", required_sku_id, &self.sku_id)?;
        if self.inventory_status != InventoryStatus::Available
            || self.active_allocation_id.is_some()
        {
            return Err(DomainError::InventoryNotAvailable(self.inventory_status));
        }
        if !self.quality_status.allows_allocation() {
            return Err(DomainError::QualityNotEligible(self.quality_status));
        }
        Ok(())
    }

    pub fn release_allocation(
        &mut self,
        allocation: &mut OutboundAllocation,
    ) -> Result<(), DomainError> {
        ensure_reference(
            "outbound_allocation.inventory_unit_id",
            &self.id,
            &allocation.inventory_unit_id,
        )?;
        let active_allocation_id = self.active_allocation_id.as_deref().unwrap_or_default();
        ensure_reference(
            "inventory_unit.active_allocation_id",
            &allocation.id,
            active_allocation_id,
        )?;
        if self.inventory_status != InventoryStatus::Reserved
            || allocation.status != AllocationStatus::Active
        {
            return Err(invalid_transition(
                "allocation",
                format!(
                    "{}+{}",
                    self.inventory_status,
                    allocation_status_name(allocation.status)
                ),
                "released",
            ));
        }

        allocation.status = AllocationStatus::Released;
        self.active_allocation_id = None;
        self.inventory_status = InventoryStatus::Available;
        self.bump_version();
        Ok(())
    }

    pub fn ship(
        &mut self,
        allocation: &mut OutboundAllocation,
        shipment_id: impl Into<String>,
        shipment_line_id: impl Into<String>,
        shipped_at: impl Into<String>,
    ) -> Result<OutboundShipmentLine, DomainError> {
        let shipment_id = required("outbound_shipment.id", shipment_id)?;
        let shipment_line_id = required("outbound_shipment_line.id", shipment_line_id)?;
        let shipped_at = required("outbound_shipment_line.shipped_at", shipped_at)?;
        ensure_reference(
            "outbound_allocation.inventory_unit_id",
            &self.id,
            &allocation.inventory_unit_id,
        )?;
        ensure_reference(
            "outbound_allocation.sku_id",
            &self.sku_id,
            &allocation.sku_id,
        )?;
        let active_allocation_id = self.active_allocation_id.as_deref().unwrap_or_default();
        ensure_reference(
            "inventory_unit.active_allocation_id",
            &allocation.id,
            active_allocation_id,
        )?;
        if self.inventory_status != InventoryStatus::Reserved
            || allocation.status != AllocationStatus::Active
        {
            return Err(invalid_transition(
                "shipment",
                format!(
                    "{}+{}",
                    self.inventory_status,
                    allocation_status_name(allocation.status)
                ),
                "shipped",
            ));
        }
        if !self.quality_status.allows_allocation() {
            return Err(DomainError::QualityNotEligible(self.quality_status));
        }

        allocation.status = AllocationStatus::Shipped;
        self.active_allocation_id = None;
        self.latest_shipment_line_id = Some(shipment_line_id.clone());
        self.inventory_status = InventoryStatus::Shipped;
        self.bump_version();
        Ok(OutboundShipmentLine {
            id: shipment_line_id,
            outbound_shipment_id: shipment_id,
            outbound_allocation_id: allocation.id.clone(),
            outbound_order_line_id: allocation.outbound_order_line_id.clone(),
            inventory_unit_id: self.id.clone(),
            barcode_snapshot: self.barcode.clone(),
            shipped_at,
            status: ShipmentLineStatus::Shipped,
            delivered_at: None,
        })
    }

    pub fn confirm_delivery(
        &mut self,
        shipment_line: &mut OutboundShipmentLine,
        confirmation_id: impl Into<String>,
        confirmation_line_id: impl Into<String>,
        confirmation_code: impl Into<String>,
        confirmed_by: impl Into<String>,
        confirmed_at: impl Into<String>,
    ) -> Result<DeliveryConfirmationLine, DomainError> {
        let confirmation_id = required("delivery_confirmation.id", confirmation_id)?;
        let confirmation_line_id = required("delivery_confirmation_line.id", confirmation_line_id)?;
        let confirmation_code =
            required("delivery_confirmation.confirmation_code", confirmation_code)?;
        let confirmed_by = required("delivery_confirmation.confirmed_by", confirmed_by)?;
        let confirmed_at = required("delivery_confirmation.confirmed_at", confirmed_at)?;
        self.ensure_current_shipment_line(shipment_line)?;
        if self.inventory_status != InventoryStatus::Shipped
            || shipment_line.status != ShipmentLineStatus::Shipped
        {
            return Err(invalid_transition(
                "delivery",
                format!(
                    "{}+{}",
                    self.inventory_status,
                    shipment_line_status_name(shipment_line.status)
                ),
                "delivered",
            ));
        }

        shipment_line.status = ShipmentLineStatus::Delivered;
        shipment_line.delivered_at = Some(confirmed_at.clone());
        self.inventory_status = InventoryStatus::Delivered;
        self.bump_version();
        Ok(DeliveryConfirmationLine {
            id: confirmation_line_id,
            delivery_confirmation_id: confirmation_id,
            outbound_shipment_line_id: shipment_line.id.clone(),
            inventory_unit_id: self.id.clone(),
            confirmation_code,
            confirmed_by,
            confirmed_at,
            outcome: DeliveryOutcome::Accepted,
        })
    }

    pub fn return_to_quarantine(
        &mut self,
        shipment_line: &mut OutboundShipmentLine,
        return_batch_id: impl Into<String>,
        return_line_id: impl Into<String>,
        reason: impl Into<String>,
        returned_at: impl Into<String>,
    ) -> Result<OutboundReturnLine, DomainError> {
        let return_batch_id = required("outbound_return_batch.id", return_batch_id)?;
        let return_line_id = required("outbound_return_line.id", return_line_id)?;
        let reason = required("outbound_return_line.reason", reason)?;
        let returned_at = required("outbound_return_line.returned_at", returned_at)?;
        self.ensure_current_shipment_line(shipment_line)?;
        if !matches!(
            self.inventory_status,
            InventoryStatus::Shipped | InventoryStatus::Delivered
        ) || !matches!(
            shipment_line.status,
            ShipmentLineStatus::Shipped | ShipmentLineStatus::Delivered
        ) {
            return Err(invalid_transition(
                "outbound return",
                format!(
                    "{}+{}",
                    self.inventory_status,
                    shipment_line_status_name(shipment_line.status)
                ),
                "quarantined",
            ));
        }

        shipment_line.status = ShipmentLineStatus::Returned;
        self.inventory_status = InventoryStatus::Quarantined;
        self.active_allocation_id = None;
        self.bump_version();
        Ok(OutboundReturnLine {
            id: return_line_id,
            outbound_return_batch_id: return_batch_id,
            original_shipment_line_id: shipment_line.id.clone(),
            inventory_unit_id: self.id.clone(),
            reason,
            returned_at,
            disposition: ReturnDisposition::Quarantined,
        })
    }

    fn ensure_current_shipment_line(
        &self,
        shipment_line: &OutboundShipmentLine,
    ) -> Result<(), DomainError> {
        ensure_reference(
            "outbound_shipment_line.inventory_unit_id",
            &self.id,
            &shipment_line.inventory_unit_id,
        )?;
        let latest_shipment_line_id = self.latest_shipment_line_id.as_deref().unwrap_or_default();
        ensure_reference(
            "inventory_unit.latest_shipment_line_id",
            latest_shipment_line_id,
            &shipment_line.id,
        )
    }

    fn bump_version(&mut self) {
        self.version = self.version.saturating_add(1);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QualityInspection {
    pub id: String,
    pub inventory_unit_id: String,
    pub kind: InspectionKind,
    pub inspector_id: String,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub status: InspectionStatus,
    pub outcome: Option<QualityOutcome>,
    pub defect_code: Option<String>,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QualityWaiver {
    pub id: String,
    pub inventory_unit_id: String,
    pub previous_status: QualityStatus,
    pub granted_by: String,
    pub reason: String,
    pub granted_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundOrderLine {
    pub id: String,
    pub outbound_order_id: String,
    pub sku_id: String,
    pub required_quantity: u32,
    pub allocated_quantity: u32,
}

impl OutboundOrderLine {
    pub fn new(
        id: impl Into<String>,
        outbound_order_id: impl Into<String>,
        sku_id: impl Into<String>,
        required_quantity: u32,
    ) -> Result<Self, DomainError> {
        if required_quantity == 0 {
            return Err(DomainError::InvalidQuantity {
                field: "outbound_order_line.required_quantity",
                value: required_quantity,
            });
        }
        Ok(Self {
            id: required("outbound_order_line.id", id)?,
            outbound_order_id: required(
                "outbound_order_line.outbound_order_id",
                outbound_order_id,
            )?,
            sku_id: required("outbound_order_line.sku_id", sku_id)?,
            required_quantity,
            allocated_quantity: 0,
        })
    }

    pub fn allocate_unit(
        &mut self,
        unit: &mut InventoryUnit,
        allocation_id: impl Into<String>,
        allocated_at: impl Into<String>,
    ) -> Result<OutboundAllocation, DomainError> {
        if self.allocated_quantity >= self.required_quantity {
            return Err(DomainError::OrderLineFullyAllocated);
        }
        unit.ensure_allocation_eligible(&self.sku_id)?;
        let allocation_id = required("outbound_allocation.id", allocation_id)?;
        let allocated_at = required("outbound_allocation.allocated_at", allocated_at)?;

        unit.active_allocation_id = Some(allocation_id.clone());
        unit.inventory_status = InventoryStatus::Reserved;
        unit.bump_version();
        self.allocated_quantity += 1;
        Ok(OutboundAllocation {
            id: allocation_id,
            outbound_order_line_id: self.id.clone(),
            inventory_unit_id: unit.id.clone(),
            sku_id: unit.sku_id.clone(),
            owner_party_id: unit.owner_party_id.clone(),
            inbound_receipt_line_id: unit.inbound_receipt_line_id.clone(),
            allocated_at,
            status: AllocationStatus::Active,
        })
    }

    pub fn release_unit(
        &mut self,
        unit: &mut InventoryUnit,
        allocation: &mut OutboundAllocation,
    ) -> Result<(), DomainError> {
        ensure_reference(
            "outbound_allocation.outbound_order_line_id",
            &self.id,
            &allocation.outbound_order_line_id,
        )?;
        unit.release_allocation(allocation)?;
        self.allocated_quantity = self.allocated_quantity.saturating_sub(1);
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundAllocation {
    pub id: String,
    pub outbound_order_line_id: String,
    pub inventory_unit_id: String,
    pub sku_id: String,
    pub owner_party_id: String,
    pub inbound_receipt_line_id: String,
    pub allocated_at: String,
    pub status: AllocationStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundShipmentLine {
    pub id: String,
    pub outbound_shipment_id: String,
    pub outbound_allocation_id: String,
    pub outbound_order_line_id: String,
    pub inventory_unit_id: String,
    pub barcode_snapshot: String,
    pub shipped_at: String,
    pub status: ShipmentLineStatus,
    pub delivered_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryConfirmationLine {
    pub id: String,
    pub delivery_confirmation_id: String,
    pub outbound_shipment_line_id: String,
    pub inventory_unit_id: String,
    pub confirmation_code: String,
    pub confirmed_by: String,
    pub confirmed_at: String,
    pub outcome: DeliveryOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundReturnLine {
    pub id: String,
    pub outbound_return_batch_id: String,
    pub original_shipment_line_id: String,
    pub inventory_unit_id: String,
    pub reason: String,
    pub returned_at: String,
    pub disposition: ReturnDisposition,
}

fn required(field: &'static str, value: impl Into<String>) -> Result<String, DomainError> {
    let value = value.into().trim().to_owned();
    if value.is_empty() {
        return Err(DomainError::EmptyField(field));
    }
    Ok(value)
}

fn trimmed_optional(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim().to_owned();
        (!value.is_empty()).then_some(value)
    })
}

fn ensure_reference(field: &'static str, expected: &str, actual: &str) -> Result<(), DomainError> {
    if expected != actual {
        return Err(DomainError::MismatchedReference {
            field,
            expected: expected.to_owned(),
            actual: actual.to_owned(),
        });
    }
    Ok(())
}

fn invalid_transition(
    aggregate: &'static str,
    from: impl Into<String>,
    to: impl Into<String>,
) -> DomainError {
    DomainError::InvalidTransition {
        aggregate,
        from: from.into(),
        to: to.into(),
    }
}

fn document_status_name(status: DocumentStatus) -> &'static str {
    match status {
        DocumentStatus::Draft => "draft",
        DocumentStatus::Posted => "posted",
        DocumentStatus::Voided => "voided",
    }
}

fn inspection_status_name(status: InspectionStatus) -> &'static str {
    match status {
        InspectionStatus::InProgress => "in_progress",
        InspectionStatus::Completed => "completed",
    }
}

fn allocation_status_name(status: AllocationStatus) -> &'static str {
    match status {
        AllocationStatus::Active => "active",
        AllocationStatus::Released => "released",
        AllocationStatus::Shipped => "shipped",
    }
}

fn shipment_line_status_name(status: ShipmentLineStatus) -> &'static str {
    match status {
        ShipmentLineStatus::Shipped => "shipped",
        ShipmentLineStatus::Delivered => "delivered",
        ShipmentLineStatus::Returned => "returned",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_unit(barcode: &str, sku_id: &str) -> InventoryUnit {
        InventoryUnit::receive(NewInventoryUnit {
            id: format!("unit-{barcode}"),
            barcode: barcode.to_owned(),
            inbound_receipt_line_id: "inbound-line-1".to_owned(),
            owner_party_id: "owner-a".to_owned(),
            sku_id: sku_id.to_owned(),
            location_id: "receiving".to_owned(),
            received_at: "2026-07-31T01:00:00Z".to_owned(),
        })
        .expect("valid inventory unit")
    }

    fn pass_initial_inspection(unit: &mut InventoryUnit) {
        let mut inspection = unit
            .begin_quality_inspection(
                "inspection-1",
                InspectionKind::Initial,
                "quality-user",
                "2026-07-31T01:01:00Z",
            )
            .expect("inspection starts");
        unit.complete_quality_inspection(
            &mut inspection,
            QualityOutcome::Passed,
            "2026-07-31T01:02:00Z",
            None,
            None,
        )
        .expect("inspection passes");
    }

    fn reserve_unit(unit: &mut InventoryUnit) -> (OutboundOrderLine, OutboundAllocation) {
        let mut order_line = OutboundOrderLine::new("order-line-1", "order-1", &unit.sku_id, 1)
            .expect("valid order line");
        let allocation = order_line
            .allocate_unit(unit, "allocation-1", "2026-07-31T02:00:00Z")
            .expect("unit allocated");
        (order_line, allocation)
    }

    fn ship_unit(
        unit: &mut InventoryUnit,
        allocation: &mut OutboundAllocation,
    ) -> OutboundShipmentLine {
        unit.ship(
            allocation,
            "shipment-1",
            "shipment-line-1",
            "2026-07-31T03:00:00Z",
        )
        .expect("unit shipped")
    }

    #[test]
    fn inbound_unit_defaults_to_received_and_untested() {
        let unit = new_unit("SN-001", "sku-x");

        assert_eq!(unit.inventory_status, InventoryStatus::Received);
        assert_eq!(unit.quality_status, QualityStatus::Untested);
        assert_eq!(unit.active_allocation_id, None);
        assert_eq!(unit.latest_shipment_line_id, None);
        assert_eq!(unit.version, 1);
    }

    #[test]
    fn inbound_unit_requires_a_real_inbound_source() {
        let error = InventoryUnit::receive(NewInventoryUnit {
            id: "unit-1".to_owned(),
            barcode: "SN-001".to_owned(),
            inbound_receipt_line_id: "  ".to_owned(),
            owner_party_id: "owner-a".to_owned(),
            sku_id: "sku-x".to_owned(),
            location_id: "receiving".to_owned(),
            received_at: "2026-07-31T01:00:00Z".to_owned(),
        })
        .expect_err("inbound source is mandatory");

        assert_eq!(
            error,
            DomainError::EmptyField("inventory_unit.inbound_receipt_line_id")
        );
    }

    #[test]
    fn failed_quality_requires_retest_before_becoming_available() {
        let mut unit = new_unit("SN-001", "sku-x");
        let mut initial = unit
            .begin_quality_inspection(
                "inspection-1",
                InspectionKind::Initial,
                "quality-user",
                "2026-07-31T01:01:00Z",
            )
            .expect("initial inspection starts");
        unit.complete_quality_inspection(
            &mut initial,
            QualityOutcome::Failed,
            "2026-07-31T01:02:00Z",
            Some("POWER".to_owned()),
            None,
        )
        .expect("failed result is recorded");

        assert_eq!(unit.inventory_status, InventoryStatus::Quarantined);
        assert_eq!(unit.quality_status, QualityStatus::Failed);
        assert!(unit.ensure_allocation_eligible("sku-x").is_err());

        let mut retest = unit
            .begin_quality_inspection(
                "inspection-2",
                InspectionKind::Retest,
                "quality-user",
                "2026-07-31T01:03:00Z",
            )
            .expect("retest starts");
        unit.complete_quality_inspection(
            &mut retest,
            QualityOutcome::Passed,
            "2026-07-31T01:04:00Z",
            None,
            None,
        )
        .expect("retest passes");

        assert_eq!(unit.inventory_status, InventoryStatus::Available);
        assert_eq!(unit.quality_status, QualityStatus::Passed);
    }

    #[test]
    fn quality_inspection_cannot_be_completed_twice() {
        let mut unit = new_unit("SN-001", "sku-x");
        let mut inspection = unit
            .begin_quality_inspection(
                "inspection-1",
                InspectionKind::Initial,
                "quality-user",
                "2026-07-31T01:01:00Z",
            )
            .expect("inspection starts");
        unit.complete_quality_inspection(
            &mut inspection,
            QualityOutcome::Passed,
            "2026-07-31T01:02:00Z",
            None,
            None,
        )
        .expect("first completion succeeds");

        assert!(matches!(
            unit.complete_quality_inspection(
                &mut inspection,
                QualityOutcome::Failed,
                "2026-07-31T01:03:00Z",
                None,
                None,
            ),
            Err(DomainError::InvalidTransition { .. })
        ));
    }

    #[test]
    fn waiver_requires_reason_and_releases_unit() {
        let mut unit = new_unit("SN-001", "sku-x");

        assert_eq!(
            unit.waive_quality("waiver-1", "supervisor", "  ", "2026-07-31T01:01:00Z"),
            Err(DomainError::EmptyField("quality_waiver.reason"))
        );

        let waiver = unit
            .waive_quality(
                "waiver-1",
                "supervisor",
                "approved for this order",
                "2026-07-31T01:01:00Z",
            )
            .expect("waiver succeeds");
        assert_eq!(waiver.previous_status, QualityStatus::Untested);
        assert_eq!(unit.quality_status, QualityStatus::Waived);
        assert_eq!(unit.inventory_status, InventoryStatus::Available);
    }

    #[test]
    fn allocation_rejects_untested_and_wrong_sku_units() {
        let mut untested = new_unit("SN-001", "sku-x");
        let mut line = OutboundOrderLine::new("order-line-1", "order-1", "sku-x", 1).unwrap();

        assert_eq!(
            line.allocate_unit(&mut untested, "allocation-1", "2026-07-31T02:00:00Z"),
            Err(DomainError::InventoryNotAvailable(
                InventoryStatus::Received
            ))
        );

        pass_initial_inspection(&mut untested);
        let mut wrong_sku_line =
            OutboundOrderLine::new("order-line-2", "order-1", "sku-y", 1).unwrap();
        assert!(matches!(
            wrong_sku_line.allocate_unit(&mut untested, "allocation-2", "2026-07-31T02:00:00Z"),
            Err(DomainError::MismatchedReference {
                field: "inventory_unit.sku_id",
                ..
            })
        ));
    }

    #[test]
    fn only_one_active_allocation_is_allowed_and_it_can_be_released() {
        let mut unit = new_unit("SN-001", "sku-x");
        pass_initial_inspection(&mut unit);
        let (mut line, mut allocation) = reserve_unit(&mut unit);
        let mut second_line =
            OutboundOrderLine::new("order-line-2", "order-2", "sku-x", 1).unwrap();

        assert_eq!(unit.inventory_status, InventoryStatus::Reserved);
        assert!(second_line
            .allocate_unit(&mut unit, "allocation-2", "2026-07-31T02:01:00Z")
            .is_err());

        line.release_unit(&mut unit, &mut allocation)
            .expect("allocation is released");
        assert_eq!(allocation.status, AllocationStatus::Released);
        assert_eq!(unit.inventory_status, InventoryStatus::Available);
        assert_eq!(unit.active_allocation_id, None);
        assert_eq!(line.allocated_quantity, 0);
    }

    #[test]
    fn shipment_requires_the_units_active_allocation() {
        let mut unit = new_unit("SN-001", "sku-x");
        pass_initial_inspection(&mut unit);
        let (_, mut allocation) = reserve_unit(&mut unit);
        allocation.id = "different-allocation".to_owned();

        assert!(matches!(
            unit.ship(
                &mut allocation,
                "shipment-1",
                "shipment-line-1",
                "2026-07-31T03:00:00Z"
            ),
            Err(DomainError::MismatchedReference {
                field: "inventory_unit.active_allocation_id",
                ..
            })
        ));
        assert_eq!(unit.inventory_status, InventoryStatus::Reserved);
    }

    #[test]
    fn shipment_and_delivery_preserve_traceability() {
        let mut unit = new_unit("SN-001", "sku-x");
        pass_initial_inspection(&mut unit);
        let (_, mut allocation) = reserve_unit(&mut unit);
        let mut shipment_line = ship_unit(&mut unit, &mut allocation);

        assert_eq!(unit.inventory_status, InventoryStatus::Shipped);
        assert_eq!(allocation.status, AllocationStatus::Shipped);
        assert_eq!(shipment_line.barcode_snapshot, "SN-001");
        assert_eq!(shipment_line.outbound_allocation_id, "allocation-1");

        let confirmation = unit
            .confirm_delivery(
                &mut shipment_line,
                "confirmation-1",
                "confirmation-line-1",
                "UPSTREAM-8899",
                "outbound-user",
                "2026-07-31T04:00:00Z",
            )
            .expect("delivery is confirmed");
        assert_eq!(unit.inventory_status, InventoryStatus::Delivered);
        assert_eq!(shipment_line.status, ShipmentLineStatus::Delivered);
        assert_eq!(confirmation.outbound_shipment_line_id, shipment_line.id);
        assert_eq!(confirmation.confirmation_code, "UPSTREAM-8899");
    }

    #[test]
    fn delivery_requires_a_confirmation_code_without_partial_mutation() {
        let mut unit = new_unit("SN-001", "sku-x");
        pass_initial_inspection(&mut unit);
        let (_, mut allocation) = reserve_unit(&mut unit);
        let mut shipment_line = ship_unit(&mut unit, &mut allocation);

        assert_eq!(
            unit.confirm_delivery(
                &mut shipment_line,
                "confirmation-1",
                "confirmation-line-1",
                " ",
                "outbound-user",
                "2026-07-31T04:00:00Z",
            ),
            Err(DomainError::EmptyField(
                "delivery_confirmation.confirmation_code"
            ))
        );
        assert_eq!(unit.inventory_status, InventoryStatus::Shipped);
        assert_eq!(shipment_line.status, ShipmentLineStatus::Shipped);
    }

    #[test]
    fn returned_delivery_links_original_shipment_and_enters_quarantine() {
        let mut unit = new_unit("SN-001", "sku-x");
        pass_initial_inspection(&mut unit);
        let (_, mut allocation) = reserve_unit(&mut unit);
        let mut shipment_line = ship_unit(&mut unit, &mut allocation);
        unit.confirm_delivery(
            &mut shipment_line,
            "confirmation-1",
            "confirmation-line-1",
            "UPSTREAM-8899",
            "outbound-user",
            "2026-07-31T04:00:00Z",
        )
        .expect("delivery is confirmed");

        let return_line = unit
            .return_to_quarantine(
                &mut shipment_line,
                "return-batch-1",
                "return-line-1",
                "receiver rejected the unit",
                "2026-07-31T05:00:00Z",
            )
            .expect("return is linked to original shipment");

        assert_eq!(unit.inventory_status, InventoryStatus::Quarantined);
        assert_eq!(unit.quality_status, QualityStatus::Passed);
        assert_eq!(shipment_line.status, ShipmentLineStatus::Returned);
        assert_eq!(return_line.original_shipment_line_id, "shipment-line-1");
        assert_eq!(return_line.disposition, ReturnDisposition::Quarantined);
        assert!(unit.ensure_allocation_eligible("sku-x").is_err());
    }

    #[test]
    fn returned_unit_must_pass_retest_before_reallocation() {
        let mut unit = new_unit("SN-001", "sku-x");
        pass_initial_inspection(&mut unit);
        let (_, mut allocation) = reserve_unit(&mut unit);
        let mut shipment_line = ship_unit(&mut unit, &mut allocation);
        unit.return_to_quarantine(
            &mut shipment_line,
            "return-batch-1",
            "return-line-1",
            "carrier damage",
            "2026-07-31T05:00:00Z",
        )
        .expect("return succeeds");

        let mut inspection = unit
            .begin_quality_inspection(
                "inspection-2",
                InspectionKind::Retest,
                "quality-user",
                "2026-07-31T05:01:00Z",
            )
            .expect("returned unit can enter retest");
        unit.complete_quality_inspection(
            &mut inspection,
            QualityOutcome::Passed,
            "2026-07-31T05:02:00Z",
            None,
            None,
        )
        .expect("retest passes");

        let mut second_order_line =
            OutboundOrderLine::new("order-line-2", "order-2", "sku-x", 1).unwrap();
        second_order_line
            .allocate_unit(&mut unit, "allocation-2", "2026-07-31T06:00:00Z")
            .expect("retested unit can be allocated again");
        assert_eq!(unit.inventory_status, InventoryStatus::Reserved);
    }

    #[test]
    fn return_rejects_a_shipment_line_from_another_unit() {
        let mut unit = new_unit("SN-001", "sku-x");
        pass_initial_inspection(&mut unit);
        let (_, mut allocation) = reserve_unit(&mut unit);
        let mut shipment_line = ship_unit(&mut unit, &mut allocation);
        shipment_line.inventory_unit_id = "unit-other".to_owned();

        assert!(matches!(
            unit.return_to_quarantine(
                &mut shipment_line,
                "return-batch-1",
                "return-line-1",
                "receiver rejected the unit",
                "2026-07-31T05:00:00Z"
            ),
            Err(DomainError::MismatchedReference {
                field: "outbound_shipment_line.inventory_unit_id",
                ..
            })
        ));
        assert_eq!(unit.inventory_status, InventoryStatus::Shipped);
    }
}
