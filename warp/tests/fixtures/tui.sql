-- A `warp.sqlite` as Warp 2026.11.11 lays it out. Each task is the `.txtpb` beside it,
-- encoded with Warp's own schema: `protoc -I apis/multi_agent/v1 --encode=warp.multi_agent.v1.Task
-- task.proto < <name>.txtpb` in warpdotdev/warp at 16db5e6.
CREATE TABLE agent_conversations (
    id INTEGER PRIMARY KEY NOT NULL,
    conversation_id TEXT NOT NULL,
    conversation_data TEXT NOT NULL,
    last_modified_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
, summary TEXT);
CREATE UNIQUE INDEX ux_agent_conversations_conversation_id ON agent_conversations (conversation_id);
CREATE TABLE agent_tasks (
    id INTEGER PRIMARY KEY NOT NULL,
    conversation_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    task BLOB NOT NULL,
    last_modified_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (conversation_id) REFERENCES agent_conversations (conversation_id)
);
CREATE UNIQUE INDEX ux_agent_tasks_task_id ON agent_tasks (task_id);
CREATE TABLE mcp_server_installations (
    id TEXT NOT NULL PRIMARY KEY,
    templatable_mcp_server TEXT NOT NULL,
    template_version_ts TIMESTAMP NOT NULL,
    variable_values TEXT NOT NULL,
    restore_running BOOLEAN NOT NULL,
    last_modified_at TIMESTAMP NOT NULL
);
INSERT INTO agent_conversations VALUES(1,'conv-failed','{}','2026-11-14 09:54:21.000000','{"initial_query": "Fix the build.", "title": "Fix the build", "initial_working_directory": "/tui"}');
INSERT INTO agent_tasks VALUES(1,'conv-failed','task-failed',X'0a0b7461736b2d6661696c6564120d46697820746865206275696c642a320a026631121a0a0e46697820746865206275696c642e12080a060a042f7475696a03726631720b08ccdfe0d7061080e59a772a1b0a0266326a03726631720b08cddfe0d7061080e59a77f201022003','2026-11-14 09:54:21.000000');
