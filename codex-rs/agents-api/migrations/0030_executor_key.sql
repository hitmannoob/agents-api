-- The executor public key a managed environment's connect token is bound to.
-- The first registration with the token records it; later registrations
-- with the token must present the same key, so a copied token cannot
-- register another executor. A new token clears it.
ALTER TABLE environments ADD COLUMN executor_key TEXT;
