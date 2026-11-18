-- Vault credential, session snapshot, webhook signing, MCP, and managed
-- environment secrets: each value encrypted (age, X25519) to the key that the
-- operator's passphrase unlocks at startup. Names are not secret.
CREATE TABLE IF NOT EXISTS secret_values (
    name TEXT PRIMARY KEY NOT NULL,
    ciphertext BLOB NOT NULL
);
