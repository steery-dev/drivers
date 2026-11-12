PRAGMA foreign_keys=OFF;
BEGIN TRANSACTION;
CREATE TABLE goose_db_version (
		id INTEGER PRIMARY KEY AUTOINCREMENT,
		version_id INTEGER NOT NULL,
		is_applied INTEGER NOT NULL,
		tstamp TIMESTAMP DEFAULT (datetime('now'))
	);
INSERT INTO goose_db_version VALUES(1,0,1,'2026-11-12 08:36:03');
INSERT INTO goose_db_version VALUES(2,20250424200609,1,'2026-11-12 08:36:03');
INSERT INTO goose_db_version VALUES(3,20250515105448,1,'2026-11-12 08:36:03');
INSERT INTO goose_db_version VALUES(4,20250624000000,1,'2026-11-12 08:36:03');
INSERT INTO goose_db_version VALUES(5,20250627000000,1,'2026-11-12 08:36:03');
INSERT INTO goose_db_version VALUES(6,20250810000000,1,'2026-11-12 08:36:03');
INSERT INTO goose_db_version VALUES(7,20250812000000,1,'2026-11-12 08:36:03');
INSERT INTO goose_db_version VALUES(8,20260127000000,1,'2026-11-12 08:36:03');
INSERT INTO goose_db_version VALUES(9,20260902000000,1,'2026-11-12 08:36:03');
INSERT INTO goose_db_version VALUES(10,20260912000000,1,'2026-11-12 08:36:03');
INSERT INTO goose_db_version VALUES(11,20260921000000,1,'2026-11-12 08:36:03');
INSERT INTO goose_db_version VALUES(12,20260928000000,1,'2026-11-12 08:36:03');
INSERT INTO goose_db_version VALUES(13,20260928000001,1,'2026-11-12 08:36:03');
CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    parent_session_id TEXT,
    title TEXT NOT NULL,
    message_count INTEGER NOT NULL DEFAULT 0 CHECK (message_count >= 0),
    prompt_tokens  INTEGER NOT NULL DEFAULT 0 CHECK (prompt_tokens >= 0),
    completion_tokens  INTEGER NOT NULL DEFAULT 0 CHECK (completion_tokens>= 0),
    cost REAL NOT NULL DEFAULT 0.0 CHECK (cost >= 0.0),
    updated_at INTEGER NOT NULL,  -- Unix timestamp in seconds
    created_at INTEGER NOT NULL   -- Unix timestamp in seconds
, summary_message_id TEXT, todos TEXT, channel TEXT);
INSERT INTO sessions VALUES('96194384-084a-4271-a8f1-4fe264e9f6d5',NULL,'Untitled Session',2,0,0,0.0,1794472564,1794472563,NULL,NULL,NULL);
CREATE TABLE files (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    path TEXT NOT NULL,
    content TEXT NOT NULL,
    version INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,  -- Unix timestamp in seconds
    updated_at INTEGER NOT NULL,  -- Unix timestamp in seconds
    FOREIGN KEY (session_id) REFERENCES sessions (id) ON DELETE CASCADE,
    UNIQUE(path, session_id, version)
);
CREATE TABLE messages (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    role TEXT NOT NULL,
    parts TEXT NOT NULL default '[]',
    model TEXT,
    created_at INTEGER NOT NULL,  -- Unix timestamp in seconds
    updated_at INTEGER NOT NULL,  -- Unix timestamp in seconds
    finished_at INTEGER, provider TEXT, is_summary_message INTEGER DEFAULT 0 NOT NULL, prism_model_id TEXT, prism_model_name TEXT, prism_hypercredit_savings REAL, prism_dollar_savings REAL,  -- Unix timestamp in seconds
    FOREIGN KEY (session_id) REFERENCES sessions (id) ON DELETE CASCADE
);
INSERT INTO messages VALUES('9b1327f7-a2d8-4b36-a675-f60bddf8c984','96194384-084a-4271-a8f1-4fe264e9f6d5','user','[{"type":"text","data":{"text":"Reply with exactly: PONG"}},{"type":"finish","data":{"reason":"stop","time":0}}]','',1794472563,1794472563,NULL,NULL,0,NULL,NULL,NULL,NULL);
INSERT INTO messages VALUES('e10c1b51-e7c1-452a-b94d-b79bca368954','96194384-084a-4271-a8f1-4fe264e9f6d5','assistant','[{"type":"finish","data":{"reason":"error","time":1794472564,"message":"Unauthorized","details":"User not found."}}]','cohere/north-mini-code:free',1794472563,1794472564,1794472564,'openrouter',0,NULL,NULL,NULL,NULL);
CREATE TABLE read_files (
    session_id TEXT NOT NULL CHECK (session_id != ''),
    path TEXT NOT NULL CHECK (path != ''),
    read_at INTEGER NOT NULL,  -- Unix timestamp in seconds when file was last read
    FOREIGN KEY (session_id) REFERENCES sessions (id) ON DELETE CASCADE,
    PRIMARY KEY (path, session_id)
);
CREATE TABLE mcp_disabled_servers (
    name TEXT PRIMARY KEY
);
CREATE TABLE mcp_enabled_servers (
    name TEXT PRIMARY KEY
);
INSERT INTO sqlite_sequence VALUES('goose_db_version',13);
CREATE TRIGGER update_sessions_updated_at
AFTER UPDATE ON sessions
BEGIN
UPDATE sessions SET updated_at = strftime('%s', 'now')
WHERE id = new.id;
END;
CREATE TRIGGER update_files_updated_at
AFTER UPDATE ON files
BEGIN
UPDATE files SET updated_at = strftime('%s', 'now')
WHERE id = new.id;
END;
CREATE TRIGGER update_messages_updated_at
AFTER UPDATE ON messages
BEGIN
UPDATE messages SET updated_at = strftime('%s', 'now')
WHERE id = new.id;
END;
CREATE TRIGGER update_session_message_count_on_insert
AFTER INSERT ON messages
BEGIN
UPDATE sessions SET
    message_count = message_count + 1
WHERE id = new.session_id;
END;
CREATE TRIGGER update_session_message_count_on_delete
AFTER DELETE ON messages
BEGIN
UPDATE sessions SET
    message_count = message_count - 1
WHERE id = old.session_id;
END;
CREATE INDEX idx_files_session_id ON files (session_id);
CREATE INDEX idx_files_path ON files (path);
CREATE INDEX idx_messages_session_id ON messages (session_id);
CREATE INDEX idx_sessions_created_at ON sessions (created_at);
CREATE INDEX idx_messages_created_at ON messages (created_at);
CREATE INDEX idx_files_created_at ON files (created_at);
CREATE INDEX idx_read_files_session_id ON read_files (session_id);
CREATE INDEX idx_read_files_path ON read_files (path);
CREATE INDEX idx_messages_role_created_at ON messages (role, created_at);
COMMIT;
