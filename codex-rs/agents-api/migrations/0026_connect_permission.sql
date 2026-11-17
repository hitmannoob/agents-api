-- Keys hold the official name of the permission to connect executors.
UPDATE api_keys
SET permissions = replace(permissions, '"api.environments.connect"', '"api.agents.environments.connect"');
