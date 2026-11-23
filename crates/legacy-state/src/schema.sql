-- workflow-core #40 (60d7d76e), including columns added by connect().
CREATE TABLE IF NOT EXISTS controller_owner (
    scope TEXT PRIMARY KEY,
    owner TEXT NOT NULL,
    token TEXT NOT NULL,
    generation INTEGER NOT NULL DEFAULT 1,
    lease_until REAL NOT NULL,
    updated REAL NOT NULL
);
CREATE TABLE IF NOT EXISTS dispatch (
    id TEXT PRIMARY KEY,
    host TEXT,
    backend TEXT DEFAULT 'tmux',
    cwd TEXT,
    task TEXT,
    created REAL,
    scope TEXT,
    role TEXT,
    current_round INTEGER DEFAULT 1,
    current_attempt TEXT,
    started REAL,
    report_dir TEXT,
    report TEXT,
    model TEXT,
    host_session TEXT,
    effort TEXT,
    network INTEGER,
    label TEXT,
    git INTEGER,
    launch_config TEXT,
    launch_receipt TEXT,
    backend_handle TEXT
);
CREATE TABLE IF NOT EXISTS dispatch_round (
    dispatch TEXT,
    round INTEGER,
    attempt TEXT,
    task TEXT,
    report TEXT,
    state TEXT DEFAULT 'running',
    result TEXT,
    created REAL,
    completed REAL,
    callback_channel TEXT,
    callback_hash TEXT,
    PRIMARY KEY (dispatch, round, attempt)
);
CREATE TABLE IF NOT EXISTS message (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    dispatch TEXT,
    recipient TEXT,
    type TEXT,
    body TEXT,
    created REAL,
    delivered INTEGER DEFAULT 0,
    message_uid TEXT,
    request_key TEXT,
    body_hash TEXT,
    state TEXT DEFAULT 'unsent',
    status_version INTEGER DEFAULT 1,
    resolver_actor TEXT,
    resolution_source TEXT,
    resolution_reason TEXT,
    resolution_evidence TEXT,
    resolved_at REAL
);
CREATE TABLE IF NOT EXISTS message_resolution (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    message INTEGER,
    actor TEXT,
    source TEXT,
    reason TEXT,
    evidence TEXT,
    body_hash TEXT,
    from_state TEXT,
    to_state TEXT,
    created REAL
);
CREATE TABLE IF NOT EXISTS screen_watch (
    dispatch TEXT PRIMARY KEY,
    screen TEXT,
    since REAL,
    reported_screen TEXT
);
CREATE TABLE IF NOT EXISTS host_error (
    dispatch TEXT PRIMARY KEY,
    turn TEXT
);
CREATE TABLE IF NOT EXISTS watchdog_fault (
    dispatch TEXT,
    round INTEGER,
    attempt TEXT,
    kind TEXT,
    fingerprint TEXT,
    PRIMARY KEY (dispatch, round, attempt, kind, fingerprint)
);
CREATE TABLE IF NOT EXISTS fault_event (
    dispatch TEXT PRIMARY KEY,
    operation TEXT,
    outcome TEXT,
    category TEXT,
    diagnostic TEXT,
    created REAL
);
CREATE TABLE IF NOT EXISTS lifecycle_event (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    dispatch TEXT,
    operation TEXT,
    round INTEGER,
    attempt TEXT,
    from_attempt TEXT,
    to_attempt TEXT,
    from_host TEXT,
    to_host TEXT,
    result TEXT,
    diagnostic TEXT,
    owner_generation INTEGER,
    owner_token TEXT,
    resolver_actor TEXT,
    resolution_evidence TEXT,
    resolution_owner_generation INTEGER,
    resolved_at REAL,
    consumed_at REAL,
    created REAL,
    creator_pid INTEGER,
    creator_start TEXT
);
CREATE TABLE IF NOT EXISTS waiter (
    pid INTEGER PRIMARY KEY,
    kind TEXT,
    since REAL,
    process_start TEXT
);
