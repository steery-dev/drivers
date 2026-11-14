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
INSERT INTO agent_conversations VALUES(1,'conv-old','{}','2026-11-14 09:38:21.000000',NULL);
INSERT INTO agent_tasks VALUES(1,'conv-old','task-old',X'0a087461736b2d6f6c6412144578706c61696e20746865206f6c6420636f64652a390a026f3112210a154578706c61696e20746865206f6c6420636f64652e12080a060a042f6f6c646a03726f31720b08a8d7e0d7061080e59a772a240a026f321a0c0a0a4974207061727365732e6a03726f31720b08a9d7e0d7061080e59a772a210a026f3312090a075468616e6b732e6a03726f32720b088cd8e0d7061080e59a772a220a026f341a0a0a0857656c636f6d652e6a03726f32720b088dd8e0d7061080e59a77','2026-11-14 09:38:21.000000');
