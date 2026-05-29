ALTER TABLE repo_seq ADD COLUMN id BIGSERIAL;

ALTER TABLE repo_seq DROP CONSTRAINT repo_seq_pkey;
ALTER TABLE repo_seq ADD PRIMARY KEY (id);

ALTER TABLE repo_seq ALTER COLUMN seq DROP DEFAULT;
ALTER TABLE repo_seq ALTER COLUMN seq DROP NOT NULL;

DROP INDEX IF EXISTS idx_repo_seq_seq;
ALTER TABLE repo_seq ADD CONSTRAINT repo_seq_seq_key UNIQUE (seq);

CREATE SEQUENCE IF NOT EXISTS firehose_seq;
SELECT setval('firehose_seq', (SELECT COALESCE(MAX(seq), 0) + 1 FROM repo_seq), false);

CREATE INDEX idx_repo_seq_unsequenced ON repo_seq (id) WHERE seq IS NULL;
