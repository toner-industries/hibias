use anyhow::Result;
use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::OnceLock;
use std::thread;
use std::time::SystemTime;

const MAX_BODY_LEN: usize = 32_768;

pub struct Event {
    pub ts_unix_ms: i64,
    pub kind: &'static str,
    pub request_id: Option<i64>,
    pub method: Option<String>,
    pub url: Option<String>,
    pub status: Option<i64>,
    pub latency_ms: Option<i64>,
    pub body: Option<String>,
    pub detail: Option<String>,
}

impl Event {
    fn new(kind: &'static str) -> Self {
        Self {
            ts_unix_ms: now_ms(),
            kind,
            request_id: None,
            method: None,
            url: None,
            status: None,
            latency_ms: None,
            body: None,
            detail: None,
        }
    }
}

/// Messages to the writer thread: an event to insert, or a flush request
/// answered once everything queued before it has been written.
enum Msg {
    Event(Event),
    Flush(Sender<()>),
}

pub struct Logger {
    tx: Sender<Msg>,
}

static LOGGER: OnceLock<Logger> = OnceLock::new();
static REQ_COUNTER: AtomicI64 = AtomicI64::new(1);

pub fn init(path: &Path) -> Result<()> {
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "
        PRAGMA journal_mode=WAL;
        PRAGMA synchronous=NORMAL;
        CREATE TABLE IF NOT EXISTS events (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            ts          TEXT    NOT NULL,
            ts_unix_ms  INTEGER NOT NULL,
            kind        TEXT    NOT NULL,
            request_id  INTEGER,
            method      TEXT,
            url         TEXT,
            status      INTEGER,
            latency_ms  INTEGER,
            body        TEXT,
            detail      TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_events_ts ON events(ts_unix_ms DESC);
        CREATE INDEX IF NOT EXISTS idx_events_req ON events(request_id);
        CREATE INDEX IF NOT EXISTS idx_events_kind ON events(kind);
        ",
    )?;

    let (tx, rx) = mpsc::channel::<Msg>();
    thread::Builder::new()
        .name("log-writer".into())
        .spawn(move || {
            let mut conn = conn;
            while let Ok(m) = rx.recv() {
                match m {
                    Msg::Event(e) => {
                        let _ = insert(&mut conn, &e);
                    }
                    Msg::Flush(ack) => {
                        let _ = ack.send(());
                    }
                }
            }
        })?;

    let logger = Logger { tx };
    LOGGER
        .set(logger)
        .map_err(|_| anyhow::anyhow!("logger already initialized"))?;
    note("logger initialized", None);
    Ok(())
}

fn insert(conn: &mut Connection, e: &Event) -> rusqlite::Result<()> {
    let ts = iso_from_ms(e.ts_unix_ms);
    conn.execute(
        "INSERT INTO events (ts, ts_unix_ms, kind, request_id, method, url, status, latency_ms, body, detail)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            ts,
            e.ts_unix_ms,
            e.kind,
            e.request_id,
            e.method,
            e.url,
            e.status,
            e.latency_ms,
            e.body,
            e.detail,
        ],
    )?;
    Ok(())
}

pub fn next_request_id() -> i64 {
    REQ_COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn send(e: Event) {
    if let Some(l) = LOGGER.get() {
        let _ = l.tx.send(Msg::Event(e));
    }
}

/// Block until every event queued so far is on disk. The writer is a detached
/// thread draining a channel; returning from `main` kills it mid-queue, so any
/// process exit without a flush silently loses the most recent events — in
/// practice the quit keypress and the "quit" note. Bounded wait so a wedged
/// writer can't hang shutdown.
pub fn flush() {
    if let Some(l) = LOGGER.get() {
        let (ack_tx, ack_rx) = mpsc::channel();
        if l.tx.send(Msg::Flush(ack_tx)).is_ok() {
            let _ = ack_rx.recv_timeout(std::time::Duration::from_secs(2));
        }
    }
}

pub fn api_req(req_id: i64, method: &str, url: &str, body: Option<&str>) {
    send(Event {
        kind: "api_req",
        request_id: Some(req_id),
        method: Some(method.to_string()),
        url: Some(url.to_string()),
        body: body.map(truncate),
        ..Event::new("api_req")
    });
}

pub fn api_resp(req_id: i64, status: u16, latency_ms: i64, body: Option<&str>) {
    send(Event {
        kind: "api_resp",
        request_id: Some(req_id),
        status: Some(status as i64),
        latency_ms: Some(latency_ms),
        body: body.map(truncate),
        ..Event::new("api_resp")
    });
}

pub fn api_err(req_id: i64, latency_ms: i64, err: &str) {
    send(Event {
        kind: "api_err",
        request_id: Some(req_id),
        latency_ms: Some(latency_ms),
        detail: Some(truncate(err)),
        ..Event::new("api_err")
    });
}

pub fn key(key_label: &str, mode: &str) {
    send(Event {
        kind: "key",
        detail: Some(format!("mode={mode} key={key_label}")),
        ..Event::new("key")
    });
}

pub fn mode_change(from: &str, to: &str) {
    send(Event {
        kind: "mode",
        detail: Some(format!("{from} -> {to}")),
        ..Event::new("mode")
    });
}

pub fn note(msg: &str, detail: Option<&str>) {
    send(Event {
        kind: "note",
        detail: Some(match detail {
            Some(d) => format!("{msg}: {d}"),
            None => msg.to_string(),
        }),
        ..Event::new("note")
    });
}

pub fn error(context: &str, err: &str) {
    send(Event {
        kind: "error",
        detail: Some(format!("{context}: {}", truncate(err))),
        ..Event::new("error")
    });
}

fn truncate(s: &str) -> String {
    if s.len() <= MAX_BODY_LEN {
        s.to_string()
    } else {
        let mut out = String::with_capacity(MAX_BODY_LEN + 32);
        out.push_str(&s[..MAX_BODY_LEN.min(s.len())]);
        out.push_str("\n…[truncated]");
        out
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn iso_from_ms(ms: i64) -> String {
    // RFC3339-ish in UTC without pulling chrono. Format: YYYY-MM-DDTHH:MM:SS.sssZ
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let (y, mo, d, h, mi, s) = gmtime(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{millis:03}Z")
}

fn gmtime(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    // Days since 1970-01-01.
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let h = (tod / 3600) as u32;
    let mi = ((tod % 3600) / 60) as u32;
    let s = (tod % 60) as u32;

    // Algorithm from Howard Hinnant, "civil_from_days".
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if mo <= 2 { y + 1 } else { y };
    (y as i32, mo, d, h, mi, s)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Regression: events queued right before process exit used to be lost —
    // the writer thread died with `main` before draining the channel, so the
    // final quit keypress never reached the DB. `flush()` must guarantee
    // everything sent before it is on disk.
    #[test]
    fn flush_makes_queued_events_durable() {
        let path =
            std::env::temp_dir().join(format!("hifi-log-flush-test-{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&path);
        init(&path).expect("init logger");

        for i in 0..50 {
            note(&format!("flush-test-marker-{i}"), None);
        }
        flush();

        let conn = Connection::open(&path).expect("open log db");
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM events WHERE detail LIKE 'flush-test-marker-%'",
                [],
                |r| r.get(0),
            )
            .expect("count");
        let _ = std::fs::remove_file(&path);
        assert_eq!(n, 50, "all events sent before flush() must be written");
    }
}
