-- Keep suppliers, goods owners, customers, and carriers in one reusable
-- party directory. Roles describe how a party is used; contact details
-- belong to the party itself.

ALTER TABLE business_parties ADD COLUMN contact_name TEXT;
ALTER TABLE business_parties ADD COLUMN phone TEXT;
ALTER TABLE business_parties ADD COLUMN wechat TEXT;
ALTER TABLE business_parties ADD COLUMN email TEXT;
ALTER TABLE business_parties ADD COLUMN address TEXT;
ALTER TABLE business_parties ADD COLUMN notes TEXT;
