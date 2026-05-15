SELECT 'CREATE DATABASE plc' WHERE NOT EXISTS (SELECT FROM pg_database WHERE datname = 'plc')\gexec
