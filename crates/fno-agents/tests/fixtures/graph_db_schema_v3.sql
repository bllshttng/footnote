-- A schema-3 graph.db, the shape origin/main wrote before schema 4, seeded
-- with the cases the v4 migration must carry: a JSON-quoted session `at`, a
-- dangling blocked_by, a relation listed on a missing node, a legacy session
-- id, a cost list that promotes and one with a naive timestamp that stays in
-- extras, provenance keys in extras, a claim, an encounter and a ruling join.
-- Read by crates/fno-agents/src/backlog/schema_v4.rs tests.
CREATE TABLE graph_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE nodes (
  id TEXT PRIMARY KEY, ordinal INTEGER NOT NULL, slug TEXT NOT NULL UNIQUE, title TEXT NOT NULL,
  kind TEXT, status TEXT NOT NULL,
  priority TEXT NOT NULL CHECK (priority IN ('p0','p1','p2','p3')), rank REAL,
  project TEXT, cwd TEXT, domain TEXT, estimate TEXT, difficulty TEXT,
  description TEXT, plan_path TEXT, parent_id TEXT, contained_in TEXT, superseded_by TEXT,
  caused_by TEXT, fixes_pr INTEGER, ownership_defect TEXT, created_at TEXT,
  touched_at TEXT, completed_at TEXT, completion_note TEXT,
  deferred_at TEXT, deferred_reason TEXT, deferred_kind TEXT,
  queued_at TEXT, queued_reason TEXT, reopened_at TEXT, reopened_reason TEXT,
  archived_at TEXT, session_id TEXT, has_brief INTEGER,
  blocks_everything INTEGER, cost_usd REAL, vision_path TEXT,
  artifact_url TEXT, extras TEXT NOT NULL DEFAULT '{}'
);
CREATE INDEX nodes_parent ON nodes(parent_id);
CREATE INDEX nodes_status ON nodes(status, project);
CREATE INDEX nodes_archive ON nodes(archived_at);
CREATE TABLE node_claims (
  node_id TEXT PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,
  locked_by TEXT, harness TEXT, harness_session TEXT, locked_at TEXT
);
CREATE TABLE node_dispatch (
  node_id TEXT PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,
  verb TEXT, brief TEXT, model TEXT
);
CREATE TABLE node_provenance (
  node_id TEXT PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,
  source TEXT, source_kind TEXT, source_project TEXT, source_session_id TEXT,
  source_harness TEXT, source_cwd TEXT, source_node_id TEXT, source_plan_path TEXT,
  source_inbox_msg TEXT, spawned_by_session TEXT, spawned_by_harness TEXT,
  spawned_by_cwd TEXT, think_session_id TEXT, think_output_path TEXT
);
CREATE TABLE supersessions (
  node_id TEXT PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,
  successor_id TEXT, cause TEXT, reason TEXT, verified_at TEXT, evidence_pr INTEGER,
  surfaces TEXT, matched_surfaces TEXT
);
CREATE TABLE nodes_raw (id TEXT PRIMARY KEY, ordinal INTEGER NOT NULL, body TEXT NOT NULL);
CREATE TABLE sessions (
  node_id TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE, seq INTEGER NOT NULL,
  phase TEXT NOT NULL, harness TEXT NOT NULL, session_id TEXT NOT NULL,
  started_at TEXT, ended_at TEXT, ended_by TEXT, effort TEXT, at TEXT, claimed_at TEXT,
  observed_model TEXT, merge_grant TEXT, extras TEXT NOT NULL DEFAULT '{}',
  PRIMARY KEY (node_id, seq)
);
CREATE INDEX sessions_by_session ON sessions(session_id, harness);
CREATE TABLE comments (
  node_id TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE, seq INTEGER NOT NULL,
  created_at TEXT, body TEXT, kind TEXT, title TEXT, details TEXT, difficulty TEXT,
  source TEXT, source_session_id TEXT, source_harness TEXT, extras TEXT NOT NULL DEFAULT '{}',
  PRIMARY KEY (node_id, seq)
);
CREATE TABLE encounters (
  node_id TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE, seq INTEGER NOT NULL,
  ts TEXT NOT NULL, evidence TEXT NOT NULL, session_id TEXT, voter_key TEXT, voter_kind TEXT,
  harness TEXT, fno_id TEXT, effort TEXT, model TEXT, extras TEXT NOT NULL DEFAULT '{}',
  PRIMARY KEY (node_id, seq)
);
CREATE TABLE pull_requests (
  node_id TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE, seq INTEGER NOT NULL,
  number INTEGER, url TEXT, merge_status TEXT, note TEXT, extras TEXT NOT NULL DEFAULT '{}',
  PRIMARY KEY (node_id, seq)
);
CREATE INDEX pull_requests_number ON pull_requests(number);
CREATE TABLE relations (
  node_id TEXT NOT NULL, related_node_id TEXT NOT NULL,
  type TEXT NOT NULL CHECK (type IN ('blocks','related','supersedes')),
  listed_on TEXT NOT NULL, seq INTEGER NOT NULL,
  PRIMARY KEY (node_id, related_node_id, type)
);
CREATE INDEX relations_inverse ON relations(related_node_id, type);
CREATE VIRTUAL TABLE nodes_fts USING fts5(
    id UNINDEXED, title, slug, description,
    content='nodes', content_rowid='rowid', tokenize='unicode61'
);
CREATE TRIGGER nodes_fts_ai AFTER INSERT ON nodes BEGIN
    INSERT INTO nodes_fts(rowid, id, title, slug, description)
    VALUES (new.rowid, new.id, new.title, new.slug, new.description);
END;
CREATE TABLE decisions (
  seq INTEGER PRIMARY KEY AUTOINCREMENT, event_id TEXT NOT NULL UNIQUE,
  event_type TEXT NOT NULL, ts TEXT NOT NULL, source TEXT, data TEXT NOT NULL
);
CREATE INDEX decisions_event_id ON decisions(event_id);
CREATE TABLE node_decisions (
  node_id TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
  event_id TEXT NOT NULL REFERENCES decisions(event_id) ON DELETE CASCADE,
  seq INTEGER NOT NULL, PRIMARY KEY (node_id, event_id)
);
CREATE INDEX node_decisions_order ON node_decisions(node_id, seq);

INSERT INTO graph_meta VALUES ('schema_version', '3'), ('version', 'sqlite:seed'),
  ('backend', 'sqlite'), ('decisions_imported', '1'), ('archive_imported_v2', '1');
INSERT INTO nodes (id, ordinal, slug, title, kind, status, priority, domain, created_at,
  touched_at, session_id, extras)
VALUES ('x-a', 0, 'a', 'Alpha', 'feature', 'ready', 'p2', 'code', '2026-09-11T00:00:00+00:00',
  '2026-09-12T00:00:00+00:00', '20260911T051456Z-cl67883-05ec5f',
  '{"child_lists_present":["sessions","encounters","blocked_by"],"request_origin":"operator_request","origin_evidence":"said so","cost_sessions":[{"session_id":"s-1","cost_usd":1.5,"timestamp":"2026-09-11T01:00:00+00:00"},{"session_id":"s-2","cost_usd":0.5}]}'),
  ('x-b', 1, 'b', 'Beta', 'bug', 'ready', 'p1', 'code', '2026-09-11T00:00:00Z', NULL, NULL,
  '{"cost_sessions":[{"session_id":"s-3","cost_usd":2.0,"timestamp":"2026-08-21T13:10:35.335800"}]}');
INSERT INTO node_claims VALUES ('x-a', 'holder-1', 'claude', 's-1', '2026-09-11T01:00:00+00:00');
INSERT INTO node_provenance (node_id, source, source_kind, source_session_id, source_harness)
VALUES ('x-a', 'idea', 'operator_request', 's-1', 'claude');
INSERT INTO sessions (node_id, seq, phase, harness, session_id, started_at, at)
VALUES ('x-a', 0, 'think', 'claude', 's-1', NULL, '"2026-07-19T16:43:13Z"'),
       ('x-a', 1, 'do', 'codex', 's-2', '2026-09-11T02:00:00Z', NULL);
INSERT INTO encounters (node_id, seq, ts, evidence, session_id, harness, model)
VALUES ('x-a', 0, '2026-09-11T03:00:00+00:00', 'cost me time', 's-2', 'codex', 'gpt-6-luna');
INSERT INTO comments (node_id, seq, created_at, body)
VALUES ('x-b', 0, '2026-09-11T00:00:00Z', 'kept');
INSERT INTO relations VALUES ('x-9999', 'x-a', 'blocks', 'x-a', 0);
INSERT INTO relations VALUES ('x-gone', 'x-b', 'related', 'x-gone', 0);
INSERT INTO decisions (event_id, event_type, ts, source, data)
VALUES ('d-one', 'operator_decision', '2026-09-16T00:00:01Z', 'target',
  '{"decision_id":"d-one","decision":"keep","subject":"x-a"}');
INSERT INTO node_decisions VALUES ('x-a', 'd-one', 0);
