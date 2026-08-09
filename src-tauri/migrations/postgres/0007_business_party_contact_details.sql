-- Keep suppliers, goods owners, customers, and carriers in one reusable
-- party directory. Roles describe how a party is used; contact details
-- belong to the party itself.

ALTER TABLE business_parties
    ADD COLUMN contact_name text,
    ADD COLUMN phone text,
    ADD COLUMN wechat text,
    ADD COLUMN email text,
    ADD COLUMN address text,
    ADD COLUMN notes text;
