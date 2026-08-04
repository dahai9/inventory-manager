-- Preserve the original offline actor while imported rows remain attributable
-- to the authenticated network user who performed the one-time upgrade.

ALTER TABLE inbound_receipts ADD COLUMN source_actor_id text;
ALTER TABLE quality_inspections ADD COLUMN source_actor_id text;
ALTER TABLE quality_waivers ADD COLUMN source_actor_id text;
ALTER TABLE outbound_orders ADD COLUMN source_actor_id text;
ALTER TABLE outbound_allocations ADD COLUMN source_actor_id text;
ALTER TABLE outbound_shipments ADD COLUMN source_actor_id text;
ALTER TABLE delivery_confirmations ADD COLUMN source_actor_id text;
ALTER TABLE outbound_return_batches ADD COLUMN source_actor_id text;
ALTER TABLE stock_movements ADD COLUMN source_actor_id text;
ALTER TABLE audit_logs ADD COLUMN source_actor_id text;

ALTER TABLE inbound_receipts ADD CONSTRAINT inbound_receipts_source_actor_check
    CHECK (source_actor_id IS NULL OR length(btrim(source_actor_id)) > 0);
ALTER TABLE quality_inspections ADD CONSTRAINT quality_inspections_source_actor_check
    CHECK (source_actor_id IS NULL OR length(btrim(source_actor_id)) > 0);
ALTER TABLE quality_waivers ADD CONSTRAINT quality_waivers_source_actor_check
    CHECK (source_actor_id IS NULL OR length(btrim(source_actor_id)) > 0);
ALTER TABLE outbound_orders ADD CONSTRAINT outbound_orders_source_actor_check
    CHECK (source_actor_id IS NULL OR length(btrim(source_actor_id)) > 0);
ALTER TABLE outbound_allocations ADD CONSTRAINT outbound_allocations_source_actor_check
    CHECK (source_actor_id IS NULL OR length(btrim(source_actor_id)) > 0);
ALTER TABLE outbound_shipments ADD CONSTRAINT outbound_shipments_source_actor_check
    CHECK (source_actor_id IS NULL OR length(btrim(source_actor_id)) > 0);
ALTER TABLE delivery_confirmations ADD CONSTRAINT delivery_confirmations_source_actor_check
    CHECK (source_actor_id IS NULL OR length(btrim(source_actor_id)) > 0);
ALTER TABLE outbound_return_batches ADD CONSTRAINT outbound_return_batches_source_actor_check
    CHECK (source_actor_id IS NULL OR length(btrim(source_actor_id)) > 0);
ALTER TABLE stock_movements ADD CONSTRAINT stock_movements_source_actor_check
    CHECK (source_actor_id IS NULL OR length(btrim(source_actor_id)) > 0);
ALTER TABLE audit_logs ADD CONSTRAINT audit_logs_source_actor_check
    CHECK (source_actor_id IS NULL OR length(btrim(source_actor_id)) > 0);

CREATE OR REPLACE FUNCTION app.reject_source_actor_mutation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.source_actor_id IS DISTINCT FROM OLD.source_actor_id THEN
        RAISE EXCEPTION 'source_actor_id is immutable on %', TG_TABLE_NAME
            USING ERRCODE = '55000';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER inbound_receipts_source_actor_immutable
BEFORE UPDATE OF source_actor_id ON inbound_receipts
FOR EACH ROW EXECUTE FUNCTION app.reject_source_actor_mutation();
CREATE TRIGGER quality_inspections_source_actor_immutable
BEFORE UPDATE OF source_actor_id ON quality_inspections
FOR EACH ROW EXECUTE FUNCTION app.reject_source_actor_mutation();
CREATE TRIGGER quality_waivers_source_actor_immutable
BEFORE UPDATE OF source_actor_id ON quality_waivers
FOR EACH ROW EXECUTE FUNCTION app.reject_source_actor_mutation();
CREATE TRIGGER outbound_orders_source_actor_immutable
BEFORE UPDATE OF source_actor_id ON outbound_orders
FOR EACH ROW EXECUTE FUNCTION app.reject_source_actor_mutation();
CREATE TRIGGER outbound_allocations_source_actor_immutable
BEFORE UPDATE OF source_actor_id ON outbound_allocations
FOR EACH ROW EXECUTE FUNCTION app.reject_source_actor_mutation();
CREATE TRIGGER outbound_shipments_source_actor_immutable
BEFORE UPDATE OF source_actor_id ON outbound_shipments
FOR EACH ROW EXECUTE FUNCTION app.reject_source_actor_mutation();
CREATE TRIGGER delivery_confirmations_source_actor_immutable
BEFORE UPDATE OF source_actor_id ON delivery_confirmations
FOR EACH ROW EXECUTE FUNCTION app.reject_source_actor_mutation();
CREATE TRIGGER outbound_return_batches_source_actor_immutable
BEFORE UPDATE OF source_actor_id ON outbound_return_batches
FOR EACH ROW EXECUTE FUNCTION app.reject_source_actor_mutation();
CREATE TRIGGER stock_movements_source_actor_immutable
BEFORE UPDATE OF source_actor_id ON stock_movements
FOR EACH ROW EXECUTE FUNCTION app.reject_source_actor_mutation();
CREATE TRIGGER audit_logs_source_actor_immutable
BEFORE UPDATE OF source_actor_id ON audit_logs
FOR EACH ROW EXECUTE FUNCTION app.reject_source_actor_mutation();
