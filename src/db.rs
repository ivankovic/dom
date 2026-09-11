/*  This file is part of the Dom smarthome app.
 *
 *  Copyright © 2026 Marko Ivankovic
 *
 *  This is anti-capitalist software, released for free use by individuals and
 *  organizations that do not operate by capitalist principles. Use is permitted
 *  by individuals working for themselves, non-profits, educational institutions,
 *  and organizations whose owners are all workers with equal equity and vote —
 *  and is not permitted to law enforcement or the military.
 *
 *  Licensed under the Anti-Capitalist Software License v1.4. See the LICENSE
 *  file for the full terms and conditions, which you must satisfy to have any
 *  licence at all.
 *
 *  Source Code: https://github.com/ivankovic/dom
 *
 *  THE SOFTWARE IS PROVIDED "AS IS", WITHOUT EXPRESS OR IMPLIED WARRANTY OF ANY
 *  KIND. IN NO EVENT SHALL THE AUTHORS BE LIABLE FOR ANY CLAIM, DAMAGES OR
 *  OTHER LIABILITY ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR
 *  THE USE OR OTHER DEALINGS IN THE SOFTWARE.
 */

use sqlx::{Row, SqlitePool, sqlite::SqliteConnectOptions};
use std::collections::HashMap;
use std::net::IpAddr;
use std::str::FromStr;

/// How long a statement waits for SQLite's write lock before failing.
///
/// SQLite has one writer at a time, so every commit here is either immediate or
/// a wait. Thirty seconds is long by the standards of a request-serving
/// application and right for this one: there is no user waiting on any of these
/// writes, and the alternative to waiting is throwing the sample away.
const MAX_WRITE_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// How long a task waits for a *connection from the pool*, as opposed to for the
/// write lock once it has one.
///
/// Necessarily longer than `MAX_WRITE_WAIT`, and that relationship is the whole
/// point of naming it. A connection held by a writer waiting out the busy
/// timeout is a connection nobody else can have, so an acquire timeout shorter
/// than the write wait would let the pool give up first — turning a wait that
/// was about to succeed into a failed query, which is exactly the outcome
/// `MAX_WRITE_WAIT` was lengthened to avoid.
const MAX_POOL_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// Connections in the pool.
///
/// Left at sqlx's own default, but stated, because "ten writers on a
/// single-writer file" is a tempting thing to cut and cutting it is wrong.
/// SQLite takes one writer at a time whatever the pool says, so the extra
/// connections are not extra writers: under WAL, readers do not block at all,
/// and a writer that cannot have the lock now waits for it (see
/// `MAX_WRITE_WAIT`) rather than spinning. What the pool size actually bounds is
/// how many things can be *in flight* at once — nine background tasks, a poll
/// loop per device, and the view's own queries.
///
/// Shrinking it therefore trades a contention that is already handled for one
/// that is not: a task that cannot get a connection fails on `MAX_POOL_WAIT`
/// with nothing to retry it. Tried at four, and the on-disk tests began timing
/// out under load — which is the same failure, arriving earlier.
const MAX_CONNECTIONS: u32 = 10;

pub async fn init(uri: &str) -> anyhow::Result<SqlitePool> {
    let pool = connect(uri).await?;
    create_devices(&pool).await?;
    create_schema(&pool).await?;
    Ok(pool)
}

/// Opens the pool, puts the database into WAL mode and tightens its permissions.
///
/// Separated from the schema below only for length — nothing here is optional,
/// and `init` is the one caller.
async fn connect(uri: &str) -> anyhow::Result<SqlitePool> {
    let opts = SqliteConnectOptions::from_str(uri)
        .map_err(|e| anyhow::anyhow!(e))?
        .create_if_missing(true)
        // With WAL, `NORMAL` stops flushing the disk on every commit and flushes
        // at checkpoints instead. What that risks is the last few committed
        // transactions on a power cut; it changes nothing about surviving the
        // *process* dying, which WAL already guarantees.
        //
        // The trade is heavily one-sided here. The data is a series sampled every
        // two seconds, so losing the last moments of it after a power cut is
        // invisible and the coarser tiers re-derive from what remains. The cost
        // of `FULL` is a disk flush per commit, forever — on an SD card that is
        // both the dominant latency and what wears the card out.
        //
        // Set on the options rather than by issuing `PRAGMA synchronous`, because
        // it is a property of a *connection*: running the pragma against the pool
        // sets it on whichever one connection happened to serve the query, and
        // leaves every other connection on the default.
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
        // Freed pages go on a list that `reclaim_free_pages` hands back a few at
        // a time, instead of the file only ever growing to its high-water mark.
        //
        // This is a property of the *file*, fixed when it is created, so it only
        // takes effect on a new database. An existing one created without it
        // stays as it is until someone runs a full `VACUUM`, which is what
        // rewrites the file format — and after which this keeps it that way.
        .auto_vacuum(sqlx::sqlite::SqliteAutoVacuum::Incremental)
        // How long a statement waits for the write lock before giving up.
        //
        // Stated rather than left at sqlx's five seconds, because five is short
        // for this workload: the rollup deletes in 20,000-row batches, and on an
        // SD card one of those can hold the lock for longer than that while nine
        // poll loops queue behind it. What a timeout expiring costs here is a
        // poll's measurements — written, dropped, and visible only in the log
        // (see `MAX_WRITE_WAIT`'s callers). Waiting is strictly better than
        // losing the sample, and nothing here is interactive: the TUI never
        // writes on the draw path.
        .busy_timeout(MAX_WRITE_WAIT);
    let pool = if uri.contains(":memory:") {
        sqlx::pool::PoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await?
    } else {
        sqlx::pool::PoolOptions::new()
            .max_connections(MAX_CONNECTIONS)
            .acquire_timeout(MAX_POOL_WAIT)
            .connect_with(opts)
            .await?
    };

    sqlx::query("PRAGMA journal_mode=WAL")
        .execute(&pool)
        .await?;

    // After WAL is enabled, so the sidecars it creates exist and are covered too.
    restrict_to_owner(uri);

    Ok(pool)
}

/// Creates `Devices`, and brings an existing one up to date.
///
/// Kept apart from the rest of the schema because the order within it matters:
/// the unique index at the end is over `fingerprint`, which the migrations above
/// it are what add to a database made before that column existed.
async fn create_devices(pool: &SqlitePool) -> anyhow::Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS Devices (
            id                  INTEGER PRIMARY KEY AUTOINCREMENT,
            type                TEXT    NOT NULL,
            name                TEXT    NOT NULL,
            ip                  TEXT    NOT NULL UNIQUE,
            poll_interval_secs  INTEGER NOT NULL DEFAULT 2,
            api_key             TEXT,
            username            TEXT,
            password            TEXT,
            label               TEXT,
            auto_mode           TEXT    NOT NULL DEFAULT 'disabled'
        )",
    )
    .execute(pool)
    .await?;

    // Idempotent migrations: silently ignored if columns already exist.
    let _ = sqlx::query("ALTER TABLE Devices ADD COLUMN label TEXT")
        .execute(pool)
        .await;
    let _ =
        sqlx::query("ALTER TABLE Devices ADD COLUMN auto_mode TEXT NOT NULL DEFAULT 'disabled'")
            .execute(pool)
            .await;
    // Stable per-device identity (currently the LAN MAC address, from the ARP
    // cache) independent of IP, so a device that gets a new DHCP lease can be
    // recognized as the same physical device rather than showing up as a
    // second, permanently-unreachable entry — see `upsert_device`.
    let _ = sqlx::query("ALTER TABLE Devices ADD COLUMN fingerprint TEXT")
        .execute(pool)
        .await;
    // SHA-256 of the TLS certificate this device presented the first time Dom
    // connected to it over HTTPS. NULL until then; see `devices::tls`. Distinct
    // from `fingerprint` above, which is the LAN MAC and identifies the hardware
    // — this identifies the key it holds.
    let _ = sqlx::query("ALTER TABLE Devices ADD COLUMN tls_fingerprint TEXT")
        .execute(pool)
        .await;

    // NULLs are distinct in a SQLite unique index, so devices without a known
    // fingerprint yet (or types that don't support one) never collide here —
    // only two rows of the same type with the *same* non-null fingerprint do.
    sqlx::query(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_devices_type_fingerprint
         ON Devices (type, fingerprint)",
    )
    .execute(pool)
    .await?;

    Ok(())
}

/// Creates every other table, each followed by the indexes over it.
///
/// All of it is `IF NOT EXISTS`, so this runs on every startup and does nothing
/// to a database that already has them.
async fn create_schema(pool: &SqlitePool) -> anyhow::Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS SwitchTimers (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            device_id  INTEGER NOT NULL REFERENCES Devices(id),
            time_hhmm  TEXT    NOT NULL,
            relay_on   INTEGER NOT NULL
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS RawDeviceMeasurements (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            device_id   INTEGER NOT NULL REFERENCES Devices(id),
            timestamp   TEXT    NOT NULL,
            metric      TEXT    NOT NULL,
            value       REAL    NOT NULL
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_raw_metric_time
         ON RawDeviceMeasurements (device_id, metric, timestamp DESC)",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS Energy (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            device_id   INTEGER NOT NULL REFERENCES Devices(id),
            timestamp   TEXT    NOT NULL,
            resolution  TEXT    NOT NULL CHECK (resolution IN ('2s', '1min', '10min')),
            metric      TEXT    NOT NULL,
            energy_ws   REAL    NOT NULL
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_energy_metric_res_time
         ON Energy (device_id, metric, resolution, timestamp DESC)",
    )
    .execute(pool)
    .await?;

    // The index above leads with `device_id`, so a query that asks "what happened
    // between these two instants", across devices, cannot use it and falls back
    // to scanning the whole table — measured at 0.5 s here and several seconds on
    // a Raspberry Pi, every sixty seconds, for the today-charts.
    //
    // Rows arrive in timestamp order, so maintaining this is an append to the
    // right-hand edge of the tree rather than a random insert: about the cheapest
    // an index can be to keep. It costs ~30 MB against a table that retention
    // holds to four days.
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_energy_time ON Energy (timestamp)")
        .execute(pool)
        .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS EnergyStorage (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            device_id   INTEGER NOT NULL REFERENCES Devices(id),
            timestamp   TEXT    NOT NULL,
            resolution  TEXT    NOT NULL CHECK (resolution IN ('2s', '1min', '10min')),
            rsoc_avg    REAL    NOT NULL
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_energystorage_res_time
         ON EnergyStorage (device_id, resolution, timestamp DESC)",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS NetworkStatusEvents (
            id                INTEGER PRIMARY KEY AUTOINCREMENT,
            ip                TEXT    NOT NULL,
            label             TEXT,
            timestamp         TEXT    NOT NULL,
            status            TEXT    NOT NULL,
            previous_status   TEXT    NOT NULL
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_networkstatusevents_time
         ON NetworkStatusEvents (timestamp DESC)",
    )
    .execute(pool)
    .await?;

    // Application settings that outlive a run, e.g. the chosen colour theme.
    // A key/value table rather than a column per setting so adding a setting
    // needs no migration.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS Config (
            key    TEXT PRIMARY KEY,
            value  TEXT NOT NULL
        )",
    )
    .execute(pool)
    .await?;

    // Per-local-day energy totals, rolled up from the 2s rows in `Energy`.
    //
    // The long-term statistics view cannot read `Energy` directly: at 2s
    // resolution a single month is already millions of rows (a bare COUNT over
    // 47 days measured at 16 seconds), and nothing prunes that table. Rolled up
    // by day, a year is a few thousand rows and every window is instant.
    //
    // `metric` here is *derived*, not a copy of `Energy.metric`: the signed
    // `grid` series is split into separate `grid_import` and `grid_export`
    // totals at rollup time, because summing a signed series per day would
    // collapse the two into a net figure that can't be separated afterwards.
    //
    // `device_id` is kept even though the statistics view sums over devices, so
    // the table stays a faithful aggregation of `Energy` and a per-device
    // breakdown needs no migration.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS EnergyDaily (
            device_id  INTEGER NOT NULL REFERENCES Devices(id),
            day        TEXT    NOT NULL,
            metric     TEXT    NOT NULL,
            energy_wh  REAL    NOT NULL,
            PRIMARY KEY (device_id, day, metric)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_energydaily_day ON EnergyDaily (day, metric)")
        .execute(pool)
        .await?;

    // Per-minute energy, rolled up from the 2s rows in `Energy`.
    //
    // The middle tier of `2s -> 1min -> daily`. Its purpose is to be the tier
    // that *survives*: the 2s series is only kept for a few days (see
    // `prune_energy_2s`), so anything a chart or a future historical view might
    // want has to be recoverable from here.
    //
    // A separate table rather than `Energy` rows with `resolution = '1min'`,
    // which the `resolution` column originally anticipated. Three reasons: the
    // aggregate-only columns below are meaningless for a 2s sample; a
    // (device, minute, metric) primary key gives idempotent upserts, which
    // `Energy` has no unique key for; and adding rows to `Energy` would grow
    // `idx_energy_metric_res_time`, already the single largest object in the
    // database at 914 MB against a 555 MB table.
    //
    // `energy_ws` stays the exact integral — summing 2s energies into a minute
    // loses no energy at all, only intra-minute shape. The positive and negative
    // parts are stored separately because a sign split is *not* tier-invariant: a
    // minute in which the battery both charged and discharged nets out, so
    // splitting after aggregation would understate both directions.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS EnergyMinute (
            device_id      INTEGER NOT NULL REFERENCES Devices(id),
            minute         TEXT    NOT NULL,
            metric         TEXT    NOT NULL,
            energy_ws      REAL    NOT NULL,
            energy_ws_pos  REAL    NOT NULL,
            energy_ws_neg  REAL    NOT NULL,
            span_secs      INTEGER NOT NULL,
            peak_w         REAL    NOT NULL,
            PRIMARY KEY (device_id, minute, metric)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_energyminute_time ON EnergyMinute (minute, metric)",
    )
    .execute(pool)
    .await?;

    // Which local days each rollup tier has already processed.
    //
    // Recorded explicitly rather than inferred from whether a tier produced rows,
    // because "produced nothing" is a legitimate outcome — a day with no battery
    // samples yields no `StorageDaily` row — and inferring would leave such a day
    // looking unprocessed forever, re-scanning all history on every pass.
    //
    // Keyed by tier so that adding a tier later backfills across existing history
    // on its own: its marker set starts empty, so every day is outstanding for it
    // while the established tiers are skipped.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS RollupProgress (
            tier  TEXT NOT NULL,
            day   TEXT NOT NULL,
            PRIMARY KEY (tier, day)
        )",
    )
    .execute(pool)
    .await?;

    // Which cluster node held the `Active` role during which wall-clock interval — see
    // SPECS.md, "High availability: a two-node active/standby cluster". `ended_at IS NULL`
    // means the epoch is still open. This is the mechanism a future reconciliation reads
    // through to decide whose measurements are canonical for a given range; it does not hold
    // any measurements itself, the same relationship `RollupProgress` above has to `EnergyDaily`.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS LeadershipEpochs (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            node_id     TEXT NOT NULL,
            started_at  TEXT NOT NULL,
            ended_at    TEXT
        )",
    )
    .execute(pool)
    .await?;
    // "Find the currently-open epoch" is the hot query (every role transition runs it); a
    // partial index keeps it to the one row that matters rather than scanning the whole table.
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_leadershipepochs_open
         ON LeadershipEpochs (ended_at) WHERE ended_at IS NULL",
    )
    .execute(pool)
    .await?;

    // Outdoor temperature readings from the nearest MeteoSwiss station.
    //
    // The station's own measurement time is the primary key, so re-fetching a
    // reading that has not been refreshed yet is a no-op rather than a duplicate —
    // the published data updates every ten minutes and the poll runs on the same
    // cadence, which will not stay in step.
    //
    // Deliberately not rolled up or pruned: at 144 rows a day this is a few
    // megabytes a decade, so the raw series is already coarse enough to keep.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS OutdoorTemperature (
            timestamp     TEXT PRIMARY KEY,
            station_id    TEXT NOT NULL,
            station_name  TEXT NOT NULL,
            value_c       REAL NOT NULL,
            altitude_m    REAL,
            distance_km   REAL
        )",
    )
    .execute(pool)
    .await?;

    // Per-local-day device temperature.
    //
    // Folded in incrementally rather than recomputed, because its source
    // (`RawDeviceMeasurements`) is pruned after 24 hours — far shorter than the
    // re-roll window. Recomputing yesterday late in the day would see only the
    // hour of it still inside that window and overwrite a complete day with a
    // sliver of it.
    //
    // `temp_sum` and `samples` are stored rather than an average so that folding
    // is exact, and `last_ts` is a watermark: only samples newer than it are
    // added, so a pass can run as often as it likes without double-counting.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS TemperatureDaily (
            device_id  INTEGER NOT NULL REFERENCES Devices(id),
            day        TEXT    NOT NULL,
            temp_min   REAL    NOT NULL,
            temp_max   REAL    NOT NULL,
            temp_sum   REAL    NOT NULL,
            samples    INTEGER NOT NULL,
            last_ts    TEXT    NOT NULL,
            PRIMARY KEY (device_id, day)
        )",
    )
    .execute(pool)
    .await?;

    // Per-local-day battery state of charge.
    //
    // `EnergyStorage` records RSOC every 2 seconds and nothing has ever read it —
    // 65 MB of rows plus a 124 MB index that no query touches. Rolled up to one
    // row per device-day so the 2s rows can be pruned without foreclosing
    // battery-health statistics later. RSOC is a state, not a flow, so min/max/avg
    // are the useful summaries rather than a sum.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS StorageDaily (
            device_id  INTEGER NOT NULL REFERENCES Devices(id),
            day        TEXT    NOT NULL,
            rsoc_min   REAL    NOT NULL,
            rsoc_max   REAL    NOT NULL,
            rsoc_avg   REAL    NOT NULL,
            samples    INTEGER NOT NULL,
            PRIMARY KEY (device_id, day)
        )",
    )
    .execute(pool)
    .await?;

    // Weather forecast for the array's plane, one row per 15-minute step.
    //
    // Rows are only ever written for steps that have not happened yet — see
    // `insert_forecast`. That is what makes the past rows a record of what was
    // *forecast* rather than of what the model now believes happened, and it is
    // the whole basis of the forecast-versus-actual comparison: overwriting a
    // past row with a fresher analysis would turn every past forecast retroactively
    // correct.
    //
    // `issued_at` is kept so a row can be read as "this is what we expected, and
    // this is when we expected it".
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS SolarForecast (
            valid_at          TEXT PRIMARY KEY,
            gti_w_m2          REAL NOT NULL,
            temperature_c     REAL NOT NULL,
            cloud_cover_pct   REAL NOT NULL,
            precipitation_mm  REAL NOT NULL,
            issued_at         TEXT NOT NULL
        )",
    )
    .execute(pool)
    .await?;

    Ok(())
}

/// Makes the database readable and writable only by the user running Dom.
///
/// `Devices` holds `api_key`, `username` and `password` in plain text: what is
/// needed to administer the routers and access points, the battery and the
/// wallbox. Created under the ambient umask that is mode 644, so every local
/// account can read the router administrator password out of the file. There is
/// no reason for anything but Dom to open it.
///
/// Applied on every start rather than only on creation, so a database that
/// already exists at 644 is corrected rather than left as it was found.
///
/// The `-wal` and `-shm` sidecars are covered because SQLite gives them the
/// database file's own permissions; they are set explicitly as well, since they
/// hold recently written rows and cost one syscall each.
///
/// Every failure is ignored deliberately. This is a hardening step, and a
/// filesystem that cannot express it — or a file owned by someone else — is a
/// reason to carry on with a warning, not to refuse to start. Unix-only, which
/// matches the supported platform.
fn restrict_to_owner(uri: &str) {
    use std::os::unix::fs::PermissionsExt;

    let Some(path) = uri.strip_prefix("sqlite://") else {
        return;
    };
    if path.contains(":memory:") || path.is_empty() {
        return;
    }
    for suffix in ["", "-wal", "-shm"] {
        let f = format!("{path}{suffix}");
        match std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log::warn!("could not restrict permissions on {f}: {e}"),
        }
    }
}

/// A device's currently configured poll interval, in seconds, or `None` if the
/// row is gone.
///
/// Read periodically by `devices::PollTicker` rather than once when a poll loop
/// starts, which is what makes a changed interval take effect without a
/// restart.
pub async fn poll_interval_secs(pool: &SqlitePool, device_id: i64) -> anyhow::Result<Option<i64>> {
    Ok(
        sqlx::query_scalar("SELECT poll_interval_secs FROM Devices WHERE id = ?")
            .bind(device_id)
            .fetch_optional(pool)
            .await?,
    )
}

/// Reads a setting, or `None` if it was never set. Callers are expected to have
/// a default rather than treating absence as an error — a fresh database has no
/// settings at all.
pub async fn get_config(pool: &SqlitePool, key: &str) -> anyhow::Result<Option<String>> {
    Ok(sqlx::query_scalar("SELECT value FROM Config WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await?)
}

/// Writes a setting, replacing any previous value for the same key.
pub async fn set_config(pool: &SqlitePool, key: &str, value: &str) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO Config (key, value) VALUES (?, ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

// ── Cluster: node identity and the leadership log ─────────────────────────────
//
// See SPECS.md, "High availability: a two-node active/standby cluster", and `crate::cluster`
// for the decision logic these support.

const CLUSTER_NODE_ID_KEY: &str = "cluster_node_id";

/// This installation's stable identity, generated once and persisted thereafter.
///
/// Every `LeadershipEpochs` row, and every future replication message, is stamped with this —
/// it has to survive reboots and IP changes, which is why it lives in `Config` rather than being
/// derived from anything about the network. A plain random hex string is enough: it only ever
/// has to be distinct from one specific peer's own id, not globally unique, so this reaches for
/// `rand` (already a dependency — `devices::ping_once` is another user of it) rather than a UUID
/// crate.
pub async fn get_or_create_cluster_node_id(pool: &SqlitePool) -> anyhow::Result<String> {
    if let Some(id) = get_config(pool, CLUSTER_NODE_ID_KEY).await? {
        return Ok(id);
    }
    let id: String = (0..8)
        .map(|_| format!("{:02x}", rand::random::<u8>()))
        .collect();
    set_config(pool, CLUSTER_NODE_ID_KEY, &id).await?;
    Ok(id)
}

const CLUSTER_KEYPAIR_KEY: &str = "cluster_keypair_pkcs8";

/// This installation's persistent Ed25519 identity, generated once and reloaded thereafter —
/// what every `cluster::HeartbeatReply` is signed with, and what a peer pins to recognize this
/// node again. Distinct from `get_or_create_cluster_node_id`: `node_id` is just a label, easy to
/// spoof; this keypair is what makes a heartbeat reply worth trusting once pinned (see
/// `cluster::verify_reply`). Stored as the PKCS8 document `ring` produces, hex-encoded — the same
/// unencrypted-at-rest trust level as every other secret this app already stores in `Config`
/// (`SUPPLY_KEYS`) or `Devices` (API keys, TLS pins), not a new, weaker link.
pub async fn get_or_create_cluster_keypair(
    pool: &SqlitePool,
) -> anyhow::Result<ring::signature::Ed25519KeyPair> {
    if let Some(hex) = get_config(pool, CLUSTER_KEYPAIR_KEY).await? {
        let pkcs8 = crate::cluster::from_hex(&hex)
            .ok_or_else(|| anyhow::anyhow!("stored keypair is not valid hex"))?;
        return ring::signature::Ed25519KeyPair::from_pkcs8(&pkcs8)
            .map_err(|e| anyhow::anyhow!("stored keypair is invalid: {e}"));
    }
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng)
        .map_err(|_| anyhow::anyhow!("could not generate a cluster keypair"))?;
    set_config(
        pool,
        CLUSTER_KEYPAIR_KEY,
        &crate::cluster::to_hex(pkcs8.as_ref()),
    )
    .await?;
    ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref())
        .map_err(|e| anyhow::anyhow!("just-generated keypair is invalid: {e}"))
}

/// Records that `node_id` became `Active` at `at`, closing whatever epoch was previously open.
///
/// Closing-then-opening in one call rather than two separate operations means a role transition
/// is always exactly one call, and a previous crash that left an epoch open (no matching "closed"
/// ever written) self-heals on the very next transition rather than needing its own recovery
/// path — there should only ever be one open epoch, but nothing here assumes that stayed true.
pub async fn open_leadership_epoch(
    pool: &SqlitePool,
    node_id: &str,
    at: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<()> {
    let t = crate::devices::ts(at);
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE LeadershipEpochs SET ended_at = ? WHERE ended_at IS NULL")
        .bind(&t)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO LeadershipEpochs (node_id, started_at, ended_at) VALUES (?, ?, NULL)")
        .bind(node_id)
        .bind(&t)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Which node held the `Active` role at `at`, or `None` if `at` falls outside every recorded
/// epoch (before the cluster's first election, or a gap left by a crash that lost its close).
///
/// Not read by anything yet — there is no replicated data to reconcile until a peer connection
/// exists — but this is the primitive a future reconciliation query reads through: never blend
/// two nodes' measurements for the same range, always take that range's data from whichever node
/// this returns. See SPECS.md for why blending is the one thing this design rules out.
pub async fn query_leadership_epoch_at(
    pool: &SqlitePool,
    at: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<Option<String>> {
    let t = crate::devices::ts(at);
    Ok(sqlx::query_scalar(
        "SELECT node_id FROM LeadershipEpochs
         WHERE started_at <= ? AND (ended_at IS NULL OR ended_at > ?)
         ORDER BY started_at DESC
         LIMIT 1",
    )
    .bind(&t)
    .bind(&t)
    .fetch_optional(pool)
    .await?)
}

const CLUSTER_PEER_ADDR_KEY: &str = "cluster_peer_addr";
const CLUSTER_IS_PRIMARY_KEY: &str = "cluster_is_primary";

/// The paired peer's heartbeat address (`host:port`), or `None` if this node has no peer
/// configured — that absence is what keeps every existing single-node install in solo mode.
///
/// Checks the `DOM_CLUSTER_PEER_ADDR` environment variable first, falling back to `Config`. Set
/// automatically by `main::cluster_discovery_task` once a human confirms a discovered peer (see
/// `set_cluster_peer_pubkey`) — manual `Config`/env-var setting remains a fallback for a peer
/// discovery can't reach (a different subnet, say), the same manually-set, no-UI pattern
/// `SUPPLY_KEYS` already uses.
pub async fn cluster_peer_addr(pool: &SqlitePool) -> anyhow::Result<Option<String>> {
    if let Ok(addr) = std::env::var("DOM_CLUSTER_PEER_ADDR") {
        return Ok(Some(addr));
    }
    get_config(pool, CLUSTER_PEER_ADDR_KEY).await
}

/// Sets the paired peer's address — called on first pairing and whenever
/// `main::cluster_discovery_task` finds the pinned peer key at a new IP. Writing here never
/// touches `DOM_CLUSTER_PEER_ADDR`, which stays a separate, standing override for the harness.
pub async fn set_cluster_peer_addr(pool: &SqlitePool, addr: &str) -> anyhow::Result<()> {
    set_config(pool, CLUSTER_PEER_ADDR_KEY, addr).await
}

/// Whether this node is the statically designated primary — the tie-break `cluster::decide_role`
/// uses for a fresh election or a still-establishing peer. Explicit rather than derived (e.g. by
/// comparing node ids) because whoever pairs two nodes already has an opinion on which is which,
/// and deriving it would need the peer's id before it could ever be known, which is exactly the
/// bootstrap ordering problem `PeerStatus::Establishing` exists to avoid. Defaults to `false`
/// (backup) when unset, since `true` on both sides is a real, permanent split brain, while
/// `false` on both is only a lesser failure — see the pairing setup note in SPECS.md: with
/// neither side flagged primary, both lose every tie-break and sit `Standby` forever once paired,
/// silently (`Establishing`'s escalation only fires on heartbeat *failure*, and these heartbeats
/// succeed) — worth setting explicitly on the intended primary rather than relying on this
/// default. See `cluster_peer_addr` for the env var/`Config` pattern.
pub async fn cluster_is_primary(pool: &SqlitePool) -> anyhow::Result<bool> {
    if let Ok(v) = std::env::var("DOM_CLUSTER_IS_PRIMARY") {
        return Ok(v == "true");
    }
    Ok(get_config(pool, CLUSTER_IS_PRIMARY_KEY).await?.as_deref() == Some("true"))
}

const CLUSTER_PEER_PUBKEY_KEY: &str = "cluster_peer_pubkey";

/// The pinned peer identity — a paired node's own hex-encoded Ed25519 public key
/// (`cluster::public_key_hex`) — or `None` if nothing is pinned yet. This, not `cluster_peer_addr`,
/// is the actual trust anchor: `main::cluster_discovery_task` updates the address on its own when
/// the pinned key is found at a new IP, exactly because the key is what identifies the peer and
/// the address is not (see `cluster::verify_reply` and `cluster::AlarmCondition::PeerIdentityMismatch`).
pub async fn cluster_peer_pubkey(pool: &SqlitePool) -> anyhow::Result<Option<String>> {
    get_config(pool, CLUSTER_PEER_PUBKEY_KEY).await
}

/// Pins `public_key` as the peer identity — called only from the one place a human (or, in the
/// Docker test harness, `DOM_CLUSTER_AUTO_PAIR`) confirms pairing. See `cluster_peer_pubkey`.
pub async fn set_cluster_peer_pubkey(pool: &SqlitePool, public_key: &str) -> anyhow::Result<()> {
    set_config(pool, CLUSTER_PEER_PUBKEY_KEY, public_key).await
}

// ── Daily energy rollup ───────────────────────────────────────────────────────

/// Metrics written into `EnergyDaily`. The first two are copied straight from
/// `Energy`; the grid pair is derived by splitting the signed `grid` series; the
/// last two are the battery-aware attribution described in `crate::energy`.
pub const DAILY_METRICS: [&str; 6] = [
    "consumption",
    "production",
    "grid_import",
    "grid_export",
    "grid_to_house",
    "grid_to_battery",
];

/// How many of the most recent days with data are re-rolled on every pass.
///
/// Two, not one. Re-rolling only the current day leaves a gap: if the app is not
/// running at the moment a day ends — closed at 23:50, opened at 00:10 — that
/// day's last minutes are never aggregated, and because a row already exists it
/// would look complete and never be revisited. Re-rolling the previous day too
/// closes that without having to track a watermark.
const DAYS_ALWAYS_REROLLED: usize = 2;

/// The UTC instants bounding a local calendar day, as DB timestamp strings:
/// `[start, end)`.
///
/// `Energy.timestamp` holds naive UTC, so a local day has to be expressed as a
/// UTC half-open range before it can be compared against an indexed column —
/// `date(timestamp, 'localtime')` would work but is not indexable, and without
/// the index a single day's aggregation reads every row that device ever wrote.
///
/// The upper bound is the *next local midnight* rather than start plus 24 hours,
/// which is what makes a 23- or 25-hour DST day come out right.
fn local_day_bounds_utc(day: chrono::NaiveDate) -> Option<(String, String)> {
    use chrono::{Local, TimeZone};

    let local_midnight = |d: chrono::NaiveDate| -> Option<chrono::DateTime<chrono::Utc>> {
        let naive = d.and_hms_opt(0, 0, 0)?;
        // A spring-forward transition can make a local wall-clock time
        // non-existent; `earliest` then gives the first instant that does exist.
        Local
            .from_local_datetime(&naive)
            .earliest()
            .map(|dt| dt.with_timezone(&chrono::Utc))
    };

    let start = local_midnight(day)?;
    let end = local_midnight(day.succ_opt()?)?;
    Some((
        start
            .naive_utc()
            .format(crate::devices::DB_TIMESTAMP_FMT)
            .to_string(),
        end.naive_utc()
            .format(crate::devices::DB_TIMESTAMP_FMT)
            .to_string(),
    ))
}

/// Aggregates one local day of one device's 2s rows into `EnergyMinute`.
///
/// Only *complete* minutes are written: `cutoff` must be the start of the current
/// UTC minute, and rows at or after it are ignored. A partial minute row would
/// otherwise be indistinguishable from a whole one, and would then be treated as
/// final once the 2s rows behind it were pruned.
///
/// Idempotent by replacement rather than accumulation, so re-rolling a day that
/// has since gained more samples corrects it.
pub async fn rollup_energy_minute(
    pool: &SqlitePool,
    device_id: i64,
    day: chrono::NaiveDate,
    cutoff: &str,
) -> anyhow::Result<u64> {
    let Some((start, end)) = local_day_bounds_utc(day) else {
        return Ok(0);
    };
    // Never look past the last complete minute, even if the day extends beyond it.
    let end = if end.as_str() < cutoff {
        end
    } else {
        cutoff.to_string()
    };
    if start >= end {
        return Ok(0);
    }

    let result = sqlx::query(
        "INSERT INTO EnergyMinute
             (device_id, minute, metric, energy_ws, energy_ws_pos, energy_ws_neg,
              span_secs, peak_w)
         SELECT device_id,
                strftime('%Y-%m-%d %H:%M:00', timestamp),
                metric,
                SUM(energy_ws),
                SUM(CASE WHEN energy_ws > 0 THEN  energy_ws ELSE 0 END),
                SUM(CASE WHEN energy_ws < 0 THEN -energy_ws ELSE 0 END),
                -- Seconds actually covered, not a nominal 60: a device that was
                -- offline for part of the minute must not have its average power
                -- diluted across time it never reported.
                COUNT(*) * 2,
                -- Each 2s row's energy is already a 2-second average power once
                -- divided by its interval; the peak is the largest of those. This
                -- is the one quantity pruning the 2s rows makes unrecoverable.
                MAX(ABS(energy_ws)) / 2.0
         FROM Energy
         WHERE device_id = ?
           AND resolution = '2s'
           AND timestamp >= ?
           AND timestamp < ?
         GROUP BY device_id, strftime('%Y-%m-%d %H:%M:00', timestamp), metric
         ON CONFLICT(device_id, minute, metric) DO UPDATE SET
             energy_ws     = excluded.energy_ws,
             energy_ws_pos = excluded.energy_ws_pos,
             energy_ws_neg = excluded.energy_ws_neg,
             span_secs     = excluded.span_secs,
             peak_w        = excluded.peak_w",
    )
    .bind(device_id)
    .bind(&start)
    .bind(&end)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Start of the current UTC minute — the cutoff for `rollup_energy_minute`.
pub fn current_minute_start() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:00").to_string()
}

/// Folds any not-yet-recorded temperature samples for one local day into
/// `TemperatureDaily`.
///
/// Incremental by design. Unlike the energy tiers, which can recompute a day
/// from a raw series retained for four days, temperature comes from
/// `RawDeviceMeasurements` and survives only 24 hours — so a day has to be
/// accumulated while it happens, and can never be rebuilt afterwards.
///
/// One statement, so the read of the watermark and the write that advances it
/// cannot interleave with another pass. Samples at or before `last_ts` are
/// excluded, which is what makes running this every few minutes safe: re-running
/// folds in nothing and changes nothing.
pub async fn rollup_temperature_day(
    pool: &SqlitePool,
    device_id: i64,
    day: chrono::NaiveDate,
) -> anyhow::Result<()> {
    let Some((start, end)) = local_day_bounds_utc(day) else {
        return Ok(());
    };
    let day_str = day.format("%Y-%m-%d").to_string();

    sqlx::query(
        "INSERT INTO TemperatureDaily
             (device_id, day, temp_min, temp_max, temp_sum, samples, last_ts)
         SELECT ?, ?, MIN(value), MAX(value), SUM(value), COUNT(*), MAX(timestamp)
         FROM RawDeviceMeasurements
         WHERE device_id = ?
           AND metric = 'temperature'
           AND timestamp >= ?
           AND timestamp < ?
           AND timestamp > COALESCE(
                 (SELECT last_ts FROM TemperatureDaily WHERE device_id = ? AND day = ?), '')
         HAVING COUNT(*) > 0
         ON CONFLICT(device_id, day) DO UPDATE SET
             temp_min = MIN(temp_min, excluded.temp_min),
             temp_max = MAX(temp_max, excluded.temp_max),
             temp_sum = temp_sum + excluded.temp_sum,
             samples  = samples  + excluded.samples,
             last_ts  = excluded.last_ts",
    )
    .bind(device_id)
    .bind(&day_str)
    .bind(device_id)
    .bind(&start)
    .bind(&end)
    .bind(device_id)
    .bind(&day_str)
    .execute(pool)
    .await?;
    Ok(())
}

/// One local day of one device's temperature, in degrees Celsius.
#[derive(Debug, Clone, PartialEq)]
pub struct DailyTemperature {
    /// Address of the reporting device. Resolved here rather than handing the
    /// view a database id it would have to map back itself.
    pub ip: std::net::IpAddr,
    pub day: chrono::NaiveDate,
    pub min_c: f64,
    pub max_c: f64,
    pub avg_c: f64,
}

/// Daily temperature summaries for the inclusive local-date range, oldest first.
pub async fn query_daily_temperature(
    pool: &SqlitePool,
    from: chrono::NaiveDate,
    to: chrono::NaiveDate,
) -> anyhow::Result<Vec<DailyTemperature>> {
    let rows = sqlx::query(
        "SELECT d.ip AS ip, t.day AS day, t.temp_min AS temp_min, t.temp_max AS temp_max,
                t.temp_sum AS temp_sum, t.samples AS samples
         FROM TemperatureDaily t
         JOIN Devices d ON d.id = t.device_id
         WHERE t.day >= ? AND t.day <= ? AND t.samples > 0
         ORDER BY t.day, d.ip",
    )
    .bind(from.format("%Y-%m-%d").to_string())
    .bind(to.format("%Y-%m-%d").to_string())
    .fetch_all(pool)
    .await?;

    let mut out = Vec::new();
    for row in rows {
        let day_str: String = row.get("day");
        let Ok(day) = chrono::NaiveDate::parse_from_str(&day_str, "%Y-%m-%d") else {
            continue;
        };
        let samples: i64 = row.get("samples");
        if samples == 0 {
            continue;
        }
        let sum: f64 = row.get("temp_sum");
        let ip_str: String = row.get("ip");
        let Ok(ip) = ip_str.parse::<std::net::IpAddr>() else {
            continue;
        };
        out.push(DailyTemperature {
            ip,
            day,
            min_c: row.get("temp_min"),
            max_c: row.get("temp_max"),
            avg_c: sum / samples as f64,
        });
    }
    Ok(out)
}

/// Summarises one local day of one device's `EnergyStorage` rows into
/// `StorageDaily`.
pub async fn rollup_storage_day(
    pool: &SqlitePool,
    device_id: i64,
    day: chrono::NaiveDate,
) -> anyhow::Result<()> {
    let Some((start, end)) = local_day_bounds_utc(day) else {
        return Ok(());
    };
    let day_str = day.format("%Y-%m-%d").to_string();

    let row = sqlx::query(
        "SELECT MIN(rsoc_avg) AS lo, MAX(rsoc_avg) AS hi, AVG(rsoc_avg) AS mean,
                COUNT(*) AS samples
         FROM EnergyStorage
         WHERE device_id = ? AND resolution = '2s' AND timestamp >= ? AND timestamp < ?",
    )
    .bind(device_id)
    .bind(&start)
    .bind(&end)
    .fetch_one(pool)
    .await?;

    let samples: i64 = row.get("samples");
    if samples == 0 {
        return Ok(());
    }
    let lo: f64 = row.get("lo");
    let hi: f64 = row.get("hi");
    let mean: f64 = row.get("mean");

    sqlx::query(
        "INSERT INTO StorageDaily (device_id, day, rsoc_min, rsoc_max, rsoc_avg, samples)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT(device_id, day) DO UPDATE SET
             rsoc_min = excluded.rsoc_min,
             rsoc_max = excluded.rsoc_max,
             rsoc_avg = excluded.rsoc_avg,
             samples  = excluded.samples",
    )
    .bind(device_id)
    .bind(&day_str)
    .bind(lo)
    .bind(hi)
    .bind(mean)
    .bind(samples)
    .execute(pool)
    .await?;
    Ok(())
}

/// Aggregates one local day of one device's minute rows into `EnergyDaily`.
///
/// Reads `EnergyMinute`, not the 2s series. Two reasons: the minute tier is the
/// one that outlives pruning, so daily totals must be reproducible from it; and
/// it is ~28x fewer rows for identical output, since summing an integral in two
/// stages is exactly lossless.
///
/// This depends on the minute tier being current for `day` — `rollup_history`
/// runs the tiers in order for exactly that reason.
///
/// Idempotent: re-rolling a day replaces its totals rather than adding to them,
/// so a partially-elapsed day can be refreshed as often as needed.
pub async fn rollup_energy_day(
    pool: &SqlitePool,
    device_id: i64,
    day: chrono::NaiveDate,
) -> anyhow::Result<()> {
    let Some((start, end)) = local_day_bounds_utc(day) else {
        return Ok(());
    };
    let day_str = day.format("%Y-%m-%d").to_string();

    let row = sqlx::query(
        "SELECT
             SUM(CASE WHEN metric = 'consumption' THEN energy_ws ELSE 0 END) / 3600.0 AS consumption,
             SUM(CASE WHEN metric = 'production'  THEN energy_ws ELSE 0 END) / 3600.0 AS production,
             -- The already-split directional parts, summed. Splitting here from a
             -- signed daily sum would be wrong: a minute that both imported and
             -- exported nets out, so the split has to happen at the finest tier
             -- and be carried upwards, which is what energy_ws_pos/neg are for.
             SUM(CASE WHEN metric = 'grid' THEN energy_ws_neg ELSE 0 END) / 3600.0 AS grid_import,
             SUM(CASE WHEN metric = 'grid' THEN energy_ws_pos ELSE 0 END) / 3600.0 AS grid_export,
             -- No ELSE, so these are NULL rather than zero on a day recorded
             -- before the battery-aware series existed. That distinction is what
             -- lets such a day fall back below instead of reporting that none of
             -- its imported energy ever reached the house.
             SUM(CASE WHEN metric = 'grid_to_house' THEN energy_ws END) / 3600.0 AS grid_to_house,
             SUM(CASE WHEN metric = 'grid_to_battery' THEN energy_ws END) / 3600.0
                 AS grid_to_battery,
             COUNT(*) AS samples
         FROM EnergyMinute
         WHERE device_id = ?
           AND minute >= ?
           AND minute < ?",
    )
    .bind(device_id)
    .bind(&start)
    .bind(&end)
    .fetch_one(pool)
    .await?;

    let samples: i64 = row.get("samples");
    if samples == 0 {
        // Nothing that day. Leave any existing rows alone rather than writing
        // zeros, so a device that was simply offline is distinguishable from one
        // that genuinely used nothing.
        return Ok(());
    }

    // A day with no battery-aware series falls back to treating every imported
    // watt-hour as having served the house, which is what `consumption -
    // grid_import` always assumed. That is exactly right over a long enough
    // window and is the best available answer for a day already in the record.
    let grid_import: Option<f64> = row.get("grid_import");
    let fallback_grid_to_house = grid_import.unwrap_or(0.0);

    for metric in DAILY_METRICS {
        let wh: Option<f64> = row.get(metric);
        let wh = match metric {
            "grid_to_house" => wh.unwrap_or(fallback_grid_to_house),
            _ => wh.unwrap_or(0.0),
        };
        sqlx::query(
            "INSERT INTO EnergyDaily (device_id, day, metric, energy_wh)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(device_id, day, metric) DO UPDATE SET energy_wh = excluded.energy_wh",
        )
        .bind(device_id)
        .bind(&day_str)
        .bind(metric)
        .bind(wh)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// The rollup tiers, in the order they must run for a given day: each reads what
/// the one before it wrote, or the 2s series in the case of the first two.
const TIERS: [&str; 4] = ["minute", "storage", "temperature", "daily"];

/// Brings every rollup tier up to date, returning how many local days it worked.
///
/// Advances `2s -> EnergyMinute`, `EnergyStorage -> StorageDaily` and
/// `2s -> EnergyDaily` together, so the tiers never drift apart: a coarser tier
/// must always cover at least as much history as the finer one, or pruning the 2s
/// series would drop data that never made it upwards.
///
/// Progress is tracked per tier in `RollupProgress`, so a newly added tier
/// backfills across history the others already cover, while they are skipped.
/// The `DAYS_ALWAYS_REROLLED` most recent days are always redone regardless.
///
/// `throttle` is awaited between days: the first run after a tier is added has the
/// whole history to work through against a live multi-gigabyte database, and
/// spreading it out keeps it from starving the poll loops of I/O.
pub async fn rollup_history(
    pool: &SqlitePool,
    throttle: std::time::Duration,
) -> anyhow::Result<usize> {
    let Some(first_day) = oldest_energy_sample_day(pool).await? else {
        return Ok(0);
    };
    let today = chrono::Local::now().date_naive();

    let mut days: Vec<chrono::NaiveDate> = Vec::new();
    let mut d = first_day;
    while d <= today {
        days.push(d);
        let Some(next) = d.succ_opt() else { break };
        d = next;
    }
    let always_from = days.len().saturating_sub(DAYS_ALWAYS_REROLLED);

    // For each day, which tiers still need to run on it.
    let mut plan: Vec<(chrono::NaiveDate, Vec<&'static str>)> = Vec::new();
    for tier in TIERS {
        let done = distinct_days(
            pool,
            "SELECT day FROM RollupProgress WHERE tier = ?",
            Some(tier),
        )
        .await?;
        for (i, day) in days.iter().enumerate() {
            let outstanding =
                i >= always_from || !done.contains(&day.format("%Y-%m-%d").to_string());
            if !outstanding {
                continue;
            }
            match plan.iter_mut().find(|(d, _)| d == day) {
                Some((_, tiers)) => tiers.push(tier),
                None => plan.push((*day, vec![tier])),
            }
        }
    }
    if plan.is_empty() {
        return Ok(0);
    }
    plan.sort_by_key(|(day, _)| *day);

    let device_ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM Devices")
        .fetch_all(pool)
        .await?;
    let cutoff = current_minute_start();

    let mut worked = 0usize;
    for (day, tiers) in plan {
        for &device_id in &device_ids {
            // Run in tier order so a day's minute rows exist before anything
            // that may later be derived from them.
            for tier in TIERS.iter().filter(|t| tiers.contains(t)) {
                match *tier {
                    "minute" => {
                        rollup_energy_minute(pool, device_id, day, &cutoff).await?;
                    }
                    "storage" => rollup_storage_day(pool, device_id, day).await?,
                    "temperature" => rollup_temperature_day(pool, device_id, day).await?,
                    "daily" => rollup_energy_day(pool, device_id, day).await?,
                    _ => {}
                }
            }
        }
        // Mark the day done for every tier that ran, whether or not it produced
        // rows. Today is deliberately marked too: it is inside the
        // always-rerolled window, so the marker never stops it being redone.
        for tier in &tiers {
            sqlx::query(
                "INSERT INTO RollupProgress (tier, day) VALUES (?, ?)
                 ON CONFLICT(tier, day) DO NOTHING",
            )
            .bind(tier)
            .bind(day.format("%Y-%m-%d").to_string())
            .execute(pool)
            .await?;
        }
        worked += 1;
        if !throttle.is_zero() {
            tokio::time::sleep(throttle).await;
        }
    }
    Ok(worked)
}

/// Local day of the oldest 2s energy sample, or `None` when there are none.
async fn oldest_energy_sample_day(pool: &SqlitePool) -> anyhow::Result<Option<chrono::NaiveDate>> {
    let oldest: Option<Option<String>> = sqlx::query_scalar("SELECT MIN(timestamp) FROM Energy")
        .fetch_optional(pool)
        .await?;
    let Some(Some(oldest)) = oldest else {
        return Ok(None);
    };
    Ok(
        chrono::NaiveDateTime::parse_from_str(&oldest, crate::devices::DB_TIMESTAMP_FMT)
            .ok()
            .map(|ndt| {
                chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(ndt, chrono::Utc)
                    .with_timezone(&chrono::Local)
                    .date_naive()
            }),
    )
}

/// Runs a query returning one `YYYY-MM-DD` column and collects it into a set.
async fn distinct_days(
    pool: &SqlitePool,
    query: &str,
    bind: Option<&str>,
) -> anyhow::Result<std::collections::HashSet<String>> {
    let mut q = sqlx::query_scalar::<_, Option<String>>(query);
    if let Some(b) = bind {
        q = q.bind(b.to_string());
    }
    Ok(q.fetch_all(pool).await?.into_iter().flatten().collect())
}

/// One local day's energy totals, summed over every device, in kWh.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DailyEnergy {
    pub day: chrono::NaiveDate,
    pub consumption_kwh: f64,
    pub production_kwh: f64,
    pub grid_import_kwh: f64,
    pub grid_export_kwh: f64,
    /// Imported energy that actually reached the house, whether directly or by
    /// way of the battery. Equal to `grid_import_kwh` for days recorded before
    /// this was tracked, and for any day the battery never charged from the grid.
    /// See `crate::energy`.
    pub grid_to_house_kwh: f64,
    /// Imported energy that went into the battery instead of the house.
    pub grid_to_battery_kwh: f64,
}

/// Daily totals for the inclusive local-date range `[from, to]`, oldest first.
///
/// Reads only `EnergyDaily`, so cost is proportional to the number of days asked
/// for rather than to the 2s rows behind them. Days with no rolled-up data are
/// absent from the result rather than returned as zeros — a gap in history is
/// not the same claim as a day of no usage, and the statistics view renders the
/// two differently.
pub async fn query_daily_energy(
    pool: &SqlitePool,
    from: chrono::NaiveDate,
    to: chrono::NaiveDate,
) -> anyhow::Result<Vec<DailyEnergy>> {
    let rows = sqlx::query(
        "SELECT day, metric, SUM(energy_wh) / 1000.0 AS kwh
         FROM EnergyDaily
         WHERE day >= ? AND day <= ?
         GROUP BY day, metric
         ORDER BY day",
    )
    .bind(from.format("%Y-%m-%d").to_string())
    .bind(to.format("%Y-%m-%d").to_string())
    .fetch_all(pool)
    .await?;

    let mut by_day: std::collections::BTreeMap<chrono::NaiveDate, DailyEnergy> =
        std::collections::BTreeMap::new();
    for row in rows {
        let day_str: String = row.get("day");
        let Ok(day) = chrono::NaiveDate::parse_from_str(&day_str, "%Y-%m-%d") else {
            continue;
        };
        let metric: String = row.get("metric");
        let kwh: f64 = row.get("kwh");
        let entry = by_day.entry(day).or_insert_with(|| DailyEnergy {
            day,
            ..Default::default()
        });
        match metric.as_str() {
            "consumption" => entry.consumption_kwh = kwh,
            "production" => entry.production_kwh = kwh,
            "grid_import" => entry.grid_import_kwh = kwh,
            "grid_export" => entry.grid_export_kwh = kwh,
            "grid_to_house" => entry.grid_to_house_kwh = kwh,
            "grid_to_battery" => entry.grid_to_battery_kwh = kwh,
            _ => {}
        }
    }
    Ok(by_day.into_values().collect())
}

/// The configured location: what the user typed, resolved to coordinates.
#[derive(Debug, Clone, PartialEq)]
pub struct Location {
    /// The address as the geocoder matched it, for display and confirmation.
    pub label: String,
    /// LV95 easting/northing, used to find the nearest weather station.
    pub east: f64,
    pub north: f64,
    pub latitude: f64,
    pub longitude: f64,
}

/// `Config` keys the location is stored under. Kept as individual settings rather
/// than one encoded blob so a value can be inspected or corrected by hand.
const LOCATION_KEYS: [&str; 5] = [
    "location_label",
    "location_east",
    "location_north",
    "location_latitude",
    "location_longitude",
];

/// The stored location, or `None` if the user has not set one.
///
/// Treats a partially-written location as absent: every field is required to find
/// a station, and half a location is not usable.
pub async fn get_location(pool: &SqlitePool) -> anyhow::Result<Option<Location>> {
    let mut values = Vec::with_capacity(LOCATION_KEYS.len());
    for key in LOCATION_KEYS {
        match get_config(pool, key).await? {
            Some(v) => values.push(v),
            None => return Ok(None),
        }
    }
    let num = |i: usize| values[i].parse::<f64>().ok();
    let (Some(east), Some(north), Some(latitude), Some(longitude)) =
        (num(1), num(2), num(3), num(4))
    else {
        return Ok(None);
    };
    Ok(Some(Location {
        label: values[0].clone(),
        east,
        north,
        latitude,
        longitude,
    }))
}

/// Stores the location, replacing any previous one.
pub async fn set_location(pool: &SqlitePool, loc: &Location) -> anyhow::Result<()> {
    let values = [
        loc.label.clone(),
        loc.east.to_string(),
        loc.north.to_string(),
        loc.latitude.to_string(),
        loc.longitude.to_string(),
    ];
    for (key, value) in LOCATION_KEYS.iter().zip(values.iter()) {
        set_config(pool, key, value).await?;
    }
    Ok(())
}

/// Records one outdoor reading. Re-recording the same measurement instant is
/// ignored, so polling faster than the station updates costs nothing.
pub async fn insert_outdoor_temperature(
    pool: &SqlitePool,
    measured_at: chrono::DateTime<chrono::Utc>,
    station_id: &str,
    station_name: &str,
    value_c: f64,
    altitude_m: Option<f64>,
    distance_km: f64,
) -> anyhow::Result<bool> {
    let result = sqlx::query(
        "INSERT OR IGNORE INTO OutdoorTemperature
             (timestamp, station_id, station_name, value_c, altitude_m, distance_km)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(crate::devices::ts(measured_at))
    .bind(station_id)
    .bind(station_name)
    .bind(value_c)
    .bind(altitude_m)
    .bind(distance_km)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Lowest and highest outdoor reading recorded for a local day, if any.
pub async fn outdoor_range_for_day(
    pool: &SqlitePool,
    day: chrono::NaiveDate,
) -> anyhow::Result<Option<(f64, f64)>> {
    let Some((start, end)) = local_day_bounds_utc(day) else {
        return Ok(None);
    };
    let row = sqlx::query(
        "SELECT MIN(value_c) AS lo, MAX(value_c) AS hi, COUNT(*) AS n
         FROM OutdoorTemperature WHERE timestamp >= ? AND timestamp < ?",
    )
    .bind(&start)
    .bind(&end)
    .fetch_one(pool)
    .await?;
    let n: i64 = row.get("n");
    if n == 0 {
        return Ok(None);
    }
    Ok(Some((row.get("lo"), row.get("hi"))))
}

/// Oldest local day that has any rolled-up energy data, or `None` when the
/// rollup table is still empty. Bounds how far back the statistics view lets you
/// browse, so Left stops at the start of history rather than walking through
/// unbounded empty periods.
pub async fn oldest_energy_day(pool: &SqlitePool) -> anyhow::Result<Option<chrono::NaiveDate>> {
    let day: Option<Option<String>> = sqlx::query_scalar("SELECT MIN(day) FROM EnergyDaily")
        .fetch_optional(pool)
        .await?;
    Ok(day
        .flatten()
        .and_then(|d| chrono::NaiveDate::parse_from_str(&d, "%Y-%m-%d").ok()))
}

/// Queries today's Energy table and returns per-minute kW averages for each metric.
/// x = hours since local midnight (e.g. 13.5 = 13:30), y = avg kW for that minute bucket.
///
/// Timestamps are stored as naive UTC strings, so "today" and the hour bucketing
/// both apply SQLite's `'localtime'` modifier — otherwise the day boundary drifts
/// by the local UTC offset (e.g. still showing "yesterday" just after local midnight).
/// Where the per-minute tier stops and the raw tail begins, for a local day.
///
/// The today-charts render per-minute, which is exactly what `EnergyMinute`
/// already holds — reading it instead of re-aggregating the 2s series turns a
/// full table scan into an index range scan, measured here at 0.498 s against
/// 0.003 s. But the minute tier only advances when the rollup runs, so the last
/// few minutes are not in it yet and have to come from the raw rows.
///
/// Returns the start of the first minute the tier does *not* cover, so callers
/// can read `EnergyMinute` below it and `Energy` at or above it without
/// double-counting the boundary minute. With nothing rolled up yet it is the
/// start of the day, and everything comes from the raw rows as it used to.
async fn minute_tier_boundary(pool: &SqlitePool, day: chrono::NaiveDate) -> Option<String> {
    let (start, end) = local_day_bounds_utc(day)?;
    let last: Option<String> =
        sqlx::query_scalar("SELECT MAX(minute) FROM EnergyMinute WHERE minute >= ? AND minute < ?")
            .bind(&start)
            .bind(&end)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();

    match last.as_deref().and_then(parse_timestamp) {
        // One minute past the last one rolled up.
        Some(t) => Some(crate::devices::ts(t + chrono::Duration::minutes(1))),
        None => Some(start),
    }
}

pub async fn query_today_energy(pool: &SqlitePool) -> anyhow::Result<crate::app::EnergyChartData> {
    use sqlx::Row;
    let today = chrono::Local::now().date_naive();
    let Some((start, end)) = local_day_bounds_utc(today) else {
        return Ok(crate::app::EnergyChartData::default());
    };
    let boundary = minute_tier_boundary(pool, today)
        .await
        .unwrap_or_else(|| start.clone());

    // Per-minute averages, taken from the tier that already holds them and topped
    // up from the raw rows for the minutes it has not reached yet. Both halves
    // are bounded by explicit instants rather than `date(timestamp,'localtime')`,
    // which no index can satisfy — that predicate alone was the full scan.
    let rows = sqlx::query(
        "SELECT t_hours, metric, SUM(ws) / SUM(secs) / 1000.0 AS avg_kw
         FROM (
             SELECT (CAST(strftime('%H', minute, 'localtime') AS REAL) * 60
                     + CAST(strftime('%M', minute, 'localtime') AS REAL)) / 60.0 AS t_hours,
                    strftime('%Y-%m-%d %H:%M', minute, 'localtime') AS slot,
                    metric, energy_ws AS ws, span_secs AS secs
             FROM EnergyMinute
             WHERE minute >= ? AND minute < ?
               AND metric IN ('consumption', 'production', 'pac', 'grid')
             UNION ALL
             SELECT (CAST(strftime('%H', timestamp, 'localtime') AS REAL) * 60
                     + CAST(strftime('%M', timestamp, 'localtime') AS REAL)) / 60.0 AS t_hours,
                    strftime('%Y-%m-%d %H:%M', timestamp, 'localtime') AS slot,
                    metric, energy_ws AS ws, 2.0 AS secs
             FROM Energy
             WHERE timestamp >= ? AND timestamp < ?
               AND resolution = '2s'
               AND metric IN ('consumption', 'production', 'pac', 'grid')
         )
         GROUP BY slot, metric
         ORDER BY t_hours",
    )
    .bind(&start)
    .bind(&boundary)
    .bind(&boundary)
    .bind(&end)
    .fetch_all(pool)
    .await?;

    let mut data = crate::app::EnergyChartData::default();
    for row in rows {
        let t: f64 = row.get("t_hours");
        let metric: String = row.get("metric");
        let kw: f64 = row.get("avg_kw");
        match metric.as_str() {
            "consumption" => data.consumption.push((t, kw)),
            "production" => data.production.push((t, kw)),
            "pac" => data.battery.push((t, kw)),
            "grid" => data.grid.push((t, kw)),
            _ => {}
        }
    }

    // Daily grid import/export totals, from the same two tiers.
    //
    // The minute tier's `energy_ws_pos`/`energy_ws_neg` carry the split, and they
    // have to be read rather than re-derived: a minute that both imported and
    // exported nets out, so splitting an aggregated value afterwards would
    // understate both directions.
    let totals = sqlx::query(
        "SELECT SUM(imported) / 3600.0 / 1000.0 AS imported_kwh,
                SUM(exported) / 3600.0 / 1000.0 AS exported_kwh
         FROM (
             SELECT energy_ws_neg AS imported, energy_ws_pos AS exported
             FROM EnergyMinute
             WHERE minute >= ? AND minute < ? AND metric = 'grid'
             UNION ALL
             SELECT CASE WHEN energy_ws < 0 THEN ABS(energy_ws) ELSE 0 END,
                    CASE WHEN energy_ws > 0 THEN energy_ws      ELSE 0 END
             FROM Energy
             WHERE timestamp >= ? AND timestamp < ?
               AND resolution = '2s' AND metric = 'grid'
         )",
    )
    .bind(&start)
    .bind(&boundary)
    .bind(&boundary)
    .bind(&end)
    .fetch_one(pool)
    .await?;
    let imported: Option<f64> = totals.get("imported_kwh");
    let exported: Option<f64> = totals.get("exported_kwh");
    data.grid_imported_kwh = imported.unwrap_or(0.0);
    data.grid_exported_kwh = exported.unwrap_or(0.0);

    Ok(data)
}

/// Returns per-device energy breakdown for today.
/// Battery: charged_kwh (pac < 0) and discharged_kwh (pac > 0) shown separately.
/// Switch: kwh from power metric (charged/discharged stay 0).
pub async fn query_device_energy_today(
    pool: &SqlitePool,
) -> anyhow::Result<std::collections::HashMap<std::net::IpAddr, crate::app::DeviceEnergyToday>> {
    use sqlx::Row;
    let today = chrono::Local::now().date_naive();
    let Some((start, end)) = local_day_bounds_utc(today) else {
        return Ok(std::collections::HashMap::new());
    };
    let boundary = minute_tier_boundary(pool, today)
        .await
        .unwrap_or_else(|| start.clone());

    // Same two-tier read as the charts, and for the same reason. The charge and
    // discharge parts come from the minute tier's own sign split rather than
    // being re-derived: a minute that both charged and discharged nets out.
    let rows = sqlx::query(
        "SELECT d.ip, d.type,
                SUM(e.charged)    / 3600.0 / 1000.0 AS kwh_charged,
                SUM(e.discharged) / 3600.0 / 1000.0 AS kwh_discharged,
                SUM(e.total)      / 3600.0 / 1000.0 AS kwh_total
         FROM (
             SELECT device_id, metric,
                    energy_ws_neg AS charged,
                    energy_ws_pos AS discharged,
                    energy_ws_neg + energy_ws_pos AS total
             FROM EnergyMinute
             WHERE minute >= ? AND minute < ?
             UNION ALL
             SELECT device_id, metric,
                    CASE WHEN energy_ws < 0 THEN ABS(energy_ws) ELSE 0 END,
                    CASE WHEN energy_ws > 0 THEN energy_ws      ELSE 0 END,
                    ABS(energy_ws)
             FROM Energy
             WHERE timestamp >= ? AND timestamp < ? AND resolution = '2s'
         ) e
         JOIN Devices d ON e.device_id = d.id
         WHERE (d.type = 'sonnen_eco8'    AND e.metric = 'pac')
            OR (d.type = 'mystrom_switch' AND e.metric = 'power')
            OR (d.type = 'keba'           AND e.metric = 'power')
         GROUP BY e.device_id",
    )
    .bind(&start)
    .bind(&boundary)
    .bind(&boundary)
    .bind(&end)
    .fetch_all(pool)
    .await?;

    let mut out = std::collections::HashMap::new();
    for row in rows {
        let ip_str: String = row.get("ip");
        let Ok(ip) = ip_str.parse::<std::net::IpAddr>() else {
            continue;
        };
        let device_type: String = row.get("type");
        let entry = if device_type == "sonnen_eco8" {
            crate::app::DeviceEnergyToday {
                kwh: 0.0,
                charged_kwh: row.get("kwh_charged"),
                discharged_kwh: row.get("kwh_discharged"),
            }
        } else {
            crate::app::DeviceEnergyToday {
                kwh: row.get("kwh_total"),
                charged_kwh: 0.0,
                discharged_kwh: 0.0,
            }
        };
        out.insert(ip, entry);
    }
    Ok(out)
}

/// Persists a network-infrastructure status transition (e.g. Router OK → LOST).
pub async fn record_network_status_event(
    pool: &SqlitePool,
    ip: std::net::IpAddr,
    label: Option<&str>,
    previous_status: &str,
    status: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO NetworkStatusEvents (ip, label, timestamp, status, previous_status)
         VALUES (?, ?, datetime('now'), ?, ?)",
    )
    .bind(ip.to_string())
    .bind(label)
    .bind(status)
    .bind(previous_status)
    .execute(pool)
    .await?;
    Ok(())
}

/// Loads the most recent network-infrastructure status events, newest first,
/// to repopulate the in-memory list shown in the Network view after a restart.
pub async fn query_recent_network_status_events(
    pool: &SqlitePool,
    limit: i64,
) -> anyhow::Result<Vec<crate::app::NetworkStatusEvent>> {
    use sqlx::Row;
    let rows = sqlx::query(
        "SELECT ip, label, timestamp, status, previous_status
         FROM NetworkStatusEvents
         ORDER BY timestamp DESC, id DESC
         LIMIT ?",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;

    let mut out = Vec::new();
    for row in rows {
        let ip_str: String = row.get("ip");
        let Ok(ip) = ip_str.parse::<std::net::IpAddr>() else {
            continue;
        };
        let ts_str: String = row.get("timestamp");
        let Some(at) =
            chrono::NaiveDateTime::parse_from_str(&ts_str, crate::devices::DB_TIMESTAMP_FMT)
                .ok()
                .map(|ndt| ndt.and_utc())
        else {
            continue;
        };
        out.push(crate::app::NetworkStatusEvent {
            ip,
            label: row.get("label"),
            previous: parse_network_status(&row.get::<String, _>("previous_status")),
            current: parse_network_status(&row.get::<String, _>("status")),
            at,
        });
    }
    Ok(out)
}

fn parse_network_status(s: &str) -> crate::app::NetworkDeviceStatus {
    match s {
        "SLOW" => crate::app::NetworkDeviceStatus::Slow,
        "DEGRADED" => crate::app::NetworkDeviceStatus::Degraded,
        "LOST" => crate::app::NetworkDeviceStatus::Lost,
        "UNKNOWN" => crate::app::NetworkDeviceStatus::Unknown,
        _ => crate::app::NetworkDeviceStatus::Ok,
    }
}

/// Deletes NetworkStatusEvents rows older than 30 days. Returns rows deleted.
pub async fn prune_network_status_events(pool: &SqlitePool) -> anyhow::Result<u64> {
    let result = sqlx::query(
        "DELETE FROM NetworkStatusEvents WHERE timestamp < datetime('now', '-30 days')",
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Queries today's modem (lte1) traffic samples and buckets them at 2-minute
/// resolution. x = hours since local midnight, y = average throughput in kbps.
/// Samples are stored as per-poll byte deltas (see `devices::mikrotik`), so
/// summing a bucket and dividing by its 120s width gives average bytes/sec.
/// Width of one bar on the Internet-traffic chart, in minutes.
///
/// Tied to how often the modem's counters are actually read: each poll records
/// the bytes since the previous one, so a bucket narrower than the poll interval
/// leaves most bars empty and divides one poll's bytes by a span it did not
/// cover, overstating the rate. Kept equal to `mikrotik`'s poll interval.
const TRAFFIC_BUCKET_MINUTES: i64 = 30;

/// The chart's bar width, for the test that pins it against the poll interval.
pub fn traffic_bucket_minutes() -> i64 {
    TRAFFIC_BUCKET_MINUTES
}

pub async fn query_internet_traffic_today(
    pool: &SqlitePool,
) -> anyhow::Result<crate::app::InternetTrafficChartData> {
    use sqlx::Row;
    let rows = sqlx::query(
        "SELECT
             bucket * CAST(? AS REAL) / 60.0 AS t_hours,
             metric,
             SUM(value) / (CAST(? AS REAL) * 60.0) AS bytes_per_sec
         FROM (
             SELECT r.metric, r.value,
                    CAST((CAST(strftime('%H', r.timestamp, 'localtime') AS INTEGER) * 60
                          + CAST(strftime('%M', r.timestamp, 'localtime') AS INTEGER)) / ? AS INTEGER) AS bucket
             FROM RawDeviceMeasurements r
             WHERE date(r.timestamp, 'localtime') = date('now', 'localtime')
               AND r.metric IN ('traffic_rx_bytes', 'traffic_tx_bytes')
         )
         GROUP BY bucket, metric
         ORDER BY bucket",
    )
    .bind(TRAFFIC_BUCKET_MINUTES)
    .bind(TRAFFIC_BUCKET_MINUTES)
    .bind(TRAFFIC_BUCKET_MINUTES)
    .fetch_all(pool)
    .await?;

    let mut data = crate::app::InternetTrafficChartData::default();
    for row in rows {
        let t: f64 = row.get("t_hours");
        let metric: String = row.get("metric");
        let bytes_per_sec: f64 = row.get("bytes_per_sec");
        let kbps = bytes_per_sec * 8.0 / 1000.0;
        match metric.as_str() {
            "traffic_rx_bytes" => data.rx_kbps.push((t, kbps)),
            "traffic_tx_bytes" => data.tx_kbps.push((t, kbps)),
            _ => {}
        }
    }
    Ok(data)
}

/// Updates (or clears) the user-assigned label for a device. Pass empty string to clear.
pub async fn update_device_label(
    pool: &SqlitePool,
    ip: std::net::IpAddr,
    label: &str,
) -> anyhow::Result<()> {
    let label_opt: Option<&str> = if label.is_empty() { None } else { Some(label) };
    sqlx::query("UPDATE Devices SET label = ? WHERE ip = ?")
        .bind(label_opt)
        .bind(ip.to_string())
        .execute(pool)
        .await?;
    Ok(())
}

/// Loads all devices with their labels from the database.
/// Returns a map from IP address to label.
pub async fn load_all_device_labels(
    pool: &SqlitePool,
) -> anyhow::Result<HashMap<IpAddr, Option<String>>> {
    let rows = sqlx::query("SELECT ip, label FROM Devices WHERE label IS NOT NULL")
        .fetch_all(pool)
        .await?;

    let mut labels = HashMap::new();
    for row in rows {
        let ip_str: String = row.get("ip");
        let ip: IpAddr = ip_str
            .parse()
            .map_err(|_| anyhow::anyhow!("bad IP in DB: {}", ip_str))?;
        let label: Option<String> = row.get("label");
        labels.insert(ip, label);
    }
    Ok(labels)
}

/// Updates the label for all NetworkStatusEvents entries for a given IP address.
/// Used when a device is renamed to update historical status events.
pub async fn update_network_status_events_label(
    pool: &SqlitePool,
    ip: IpAddr,
    label: Option<&str>,
) -> anyhow::Result<()> {
    sqlx::query("UPDATE NetworkStatusEvents SET label = ? WHERE ip = ?")
        .bind(label)
        .bind(ip.to_string())
        .execute(pool)
        .await?;
    Ok(())
}

/// Loads auto-mode settings and scheduled timers for all myStrom switch devices.
pub async fn load_switch_configs(
    pool: &SqlitePool,
) -> anyhow::Result<(
    std::collections::HashMap<std::net::IpAddr, crate::app::SwitchAutoMode>,
    std::collections::HashMap<std::net::IpAddr, Vec<crate::app::SwitchTimer>>,
)> {
    use sqlx::Row;
    let device_rows =
        sqlx::query("SELECT ip, auto_mode FROM Devices WHERE type = 'mystrom_switch'")
            .fetch_all(pool)
            .await?;

    let mut modes = std::collections::HashMap::new();
    for row in &device_rows {
        let ip_str: String = row.get("ip");
        let Ok(ip) = ip_str.parse::<std::net::IpAddr>() else {
            continue;
        };
        let mode_str: String = row.get("auto_mode");
        let mode = match mode_str.as_str() {
            "time" => crate::app::SwitchAutoMode::Time,
            "eco" => crate::app::SwitchAutoMode::Eco,
            _ => crate::app::SwitchAutoMode::Disabled,
        };
        modes.insert(ip, mode);
    }

    let timer_rows = sqlx::query(
        "SELECT t.id, t.time_hhmm, t.relay_on, d.ip
         FROM SwitchTimers t
         JOIN Devices d ON t.device_id = d.id
         WHERE d.type = 'mystrom_switch'
         ORDER BY d.ip, t.time_hhmm",
    )
    .fetch_all(pool)
    .await?;

    let mut timers: std::collections::HashMap<std::net::IpAddr, Vec<crate::app::SwitchTimer>> =
        std::collections::HashMap::new();
    for row in timer_rows {
        let ip_str: String = row.get("ip");
        let Ok(ip) = ip_str.parse::<std::net::IpAddr>() else {
            continue;
        };
        let relay_on_i: i32 = row.get("relay_on");
        timers.entry(ip).or_default().push(crate::app::SwitchTimer {
            id: row.get("id"),
            time_hhmm: row.get("time_hhmm"),
            relay_on: relay_on_i != 0,
        });
    }

    Ok((modes, timers))
}

/// Persists the auto-mode setting for a switch device.
pub async fn set_switch_auto_mode(
    pool: &SqlitePool,
    ip: std::net::IpAddr,
    mode: &crate::app::SwitchAutoMode,
) -> anyhow::Result<()> {
    let mode_str = match mode {
        crate::app::SwitchAutoMode::Disabled => "disabled",
        crate::app::SwitchAutoMode::Time => "time",
        crate::app::SwitchAutoMode::Eco => "eco",
    };
    sqlx::query("UPDATE Devices SET auto_mode = ? WHERE ip = ?")
        .bind(mode_str)
        .bind(ip.to_string())
        .execute(pool)
        .await?;
    Ok(())
}

/// Loads a KEBA wallbox's persisted charging mode. Reuses the same generic
/// `auto_mode` column switches use for their own, unrelated auto-mode setting —
/// safe since each device type is always queried by its own `type`, and the
/// column's schema default ('disabled') already matches ChargingMode::Disabled.
pub async fn load_keba_mode(
    pool: &SqlitePool,
    ip: std::net::IpAddr,
) -> anyhow::Result<crate::devices::keba::ChargingMode> {
    let mode_str: String = sqlx::query_scalar("SELECT auto_mode FROM Devices WHERE ip = ?")
        .bind(ip.to_string())
        .fetch_one(pool)
        .await?;
    Ok(crate::devices::keba::ChargingMode::from_db_str(&mode_str))
}

/// Persists a KEBA wallbox's charging mode.
pub async fn set_keba_mode(
    pool: &SqlitePool,
    ip: std::net::IpAddr,
    mode: crate::devices::keba::ChargingMode,
) -> anyhow::Result<()> {
    sqlx::query("UPDATE Devices SET auto_mode = ? WHERE ip = ?")
        .bind(mode.as_db_str())
        .bind(ip.to_string())
        .execute(pool)
        .await?;
    Ok(())
}

pub struct SonnenAvgs {
    pub production_w: f64,
    pub consumption_w: f64,
    /// Sonnen's own sign convention: positive = battery discharging, negative
    /// = charging.
    pub pac_w: f64,
}

/// Average production/consumption/battery-charge readings from the last
/// `window_secs` seconds of Sonnen samples. Used by KEBA Eco mode to estimate
/// how much power is left over for the car once the house and the battery
/// have taken their share. Returns `None` if any of the three metrics has no
/// samples in the window (e.g. right after startup).
pub async fn query_recent_sonnen_avgs(
    pool: &SqlitePool,
    window_secs: i64,
) -> anyhow::Result<Option<SonnenAvgs>> {
    use sqlx::Row;
    let row = sqlx::query(
        "SELECT
             AVG(CASE WHEN r.metric = 'production' THEN r.value END) AS production,
             AVG(CASE WHEN r.metric = 'consumption' THEN r.value END) AS consumption,
             AVG(CASE WHEN r.metric = 'pac' THEN r.value END) AS pac
         FROM RawDeviceMeasurements r
         JOIN Devices d ON r.device_id = d.id
         WHERE d.type = 'sonnen_eco8'
           AND r.timestamp >= datetime('now', ? || ' seconds')",
    )
    .bind(format!("-{window_secs}"))
    .fetch_one(pool)
    .await?;

    let (production, consumption, pac): (Option<f64>, Option<f64>, Option<f64>) = (
        row.get("production"),
        row.get("consumption"),
        row.get("pac"),
    );
    Ok(match (production, consumption, pac) {
        (Some(production_w), Some(consumption_w), Some(pac_w)) => Some(SonnenAvgs {
            production_w,
            consumption_w,
            pac_w,
        }),
        _ => None,
    })
}

/// Average of a single device's own metric over the last `window_secs`
/// seconds. Used by KEBA Eco mode to get the car's *windowed* power draw over
/// the same interval as `query_recent_sonnen_avgs`, rather than a single live
/// snapshot — averaging both over the same window is what makes the "add the
/// car back in" arithmetic in `eco_decision` a stable fixed point instead of
/// a moving target (see that function's docs). Returns `None` if there are no
/// samples for this device/metric in the window.
pub async fn query_recent_device_metric_avg(
    pool: &SqlitePool,
    device_id: i64,
    metric: &str,
    window_secs: i64,
) -> anyhow::Result<Option<f64>> {
    let avg: Option<f64> = sqlx::query_scalar(
        "SELECT AVG(value)
         FROM RawDeviceMeasurements
         WHERE device_id = ?
           AND metric = ?
           AND timestamp >= datetime('now', ? || ' seconds')",
    )
    .bind(device_id)
    .bind(metric)
    .bind(format!("-{window_secs}"))
    .fetch_one(pool)
    .await?;
    Ok(avg)
}

/// Inserts a new timer for a switch device. Returns the new row's id.
pub async fn add_switch_timer(
    pool: &SqlitePool,
    ip: std::net::IpAddr,
    time_hhmm: &str,
    relay_on: bool,
) -> anyhow::Result<i64> {
    let result = sqlx::query(
        "INSERT INTO SwitchTimers (device_id, time_hhmm, relay_on)
         SELECT id, ?, ? FROM Devices WHERE ip = ?",
    )
    .bind(time_hhmm)
    .bind(relay_on as i32)
    .bind(ip.to_string())
    .execute(pool)
    .await?;
    Ok(result.last_insert_rowid())
}

/// Deletes a switch timer by its primary key id.
pub async fn delete_switch_timer(pool: &SqlitePool, timer_id: i64) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM SwitchTimers WHERE id = ?")
        .bind(timer_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Sets (or updates) the username/password login for a device by IP. Used for
/// devices authenticating with HTTP Basic Auth (e.g. MikroTik). Credentials
/// are configured here rather than passed through source code, so each
/// device can hold its own independent login.
pub async fn set_device_login(
    pool: &SqlitePool,
    ip: std::net::IpAddr,
    username: &str,
    password: &str,
) -> anyhow::Result<()> {
    sqlx::query("UPDATE Devices SET username = ?, password = ? WHERE ip = ?")
        .bind(username)
        .bind(password)
        .bind(ip.to_string())
        .execute(pool)
        .await?;
    Ok(())
}

/// Sets (or updates) the API key for a device by IP (e.g. Sonnen's Auth-Token).
pub async fn set_device_api_key(
    pool: &SqlitePool,
    ip: std::net::IpAddr,
    api_key: &str,
) -> anyhow::Result<()> {
    sqlx::query("UPDATE Devices SET api_key = ? WHERE ip = ?")
        .bind(api_key)
        .bind(ip.to_string())
        .execute(pool)
        .await?;
    Ok(())
}

/// Deletes RawDeviceMeasurements rows older than 24 hours. Returns rows deleted.
/// How many local days of 2-second samples are kept, counting today.
///
/// Must comfortably exceed both consumers of the raw series: the "today" queries
/// (which never look further back than the current local day) and
/// `DAYS_ALWAYS_REROLLED`, since a day still inside the re-roll window must still
/// have its source data or re-rolling would replace a complete total with a
/// partial one. Four days leaves two days of margin over the two-day re-roll
/// window.
pub const RAW_ENERGY_RETENTION_DAYS: i64 = 4;

/// Rows deleted per statement while pruning.
///
/// The first prune after this shipped has ~11M rows to remove. Doing that in one
/// statement would be a single enormous transaction: a multi-gigabyte WAL, and a
/// write lock held for minutes against a database the poll loops are writing to
/// every two seconds. Batching keeps each transaction short.
const PRUNE_BATCH: i64 = 20_000;

/// Deletes 2s rows older than `RAW_ENERGY_RETENTION_DAYS`, but only for local days
/// the minute tier has already processed.
///
/// The `RollupProgress` check is the safety interlock: it makes it impossible to
/// delete raw samples that were never rolled up, independently of whether the
/// caller remembered to roll up first. Without it, a rollup failure followed by a
/// prune would silently lose data permanently.
///
/// Deletes at most `PRUNE_BATCH * max_batches` rows per call so one pass cannot
/// monopolise the database; the remainder goes on the next pass.
pub async fn prune_energy_2s(pool: &SqlitePool, max_batches: usize) -> anyhow::Result<u64> {
    prune_raw_series(pool, "Energy", max_batches).await
}

/// As `prune_energy_2s`, for the RSOC series rolled up into `StorageDaily`.
pub async fn prune_energy_storage_2s(pool: &SqlitePool, max_batches: usize) -> anyhow::Result<u64> {
    prune_raw_series(pool, "EnergyStorage", max_batches).await
}

/// How many local days of per-minute rows are kept, counting today.
///
/// The minute tier is what makes per-minute history available beyond the few days
/// of raw samples; past this window the daily tier, which is kept forever, is the
/// record. Chosen so that "the last three months at minute resolution" is
/// available while the tier stays bounded — it grows about 1.2 MB a day, so
/// keeping it forever would add roughly half a gigabyte a year for detail nothing
/// currently displays.
pub const MINUTE_RETENTION_DAYS: i64 = 90;

/// Deletes minute rows older than `MINUTE_RETENTION_DAYS`, for local days the
/// daily tier has already processed.
///
/// Same interlock as the raw prune, one tier up: the daily tier is what has to
/// outlive these rows, so a day it has not summarised cannot be dropped.
pub async fn prune_energy_minute(pool: &SqlitePool, max_batches: usize) -> anyhow::Result<u64> {
    let cutoff = (chrono::Local::now().date_naive()
        - chrono::Duration::days(MINUTE_RETENTION_DAYS - 1))
    .format("%Y-%m-%d")
    .to_string();

    let mut removed = 0u64;
    for _ in 0..max_batches {
        let n = sqlx::query(
            "DELETE FROM EnergyMinute WHERE rowid IN (
                 SELECT rowid FROM EnergyMinute
                 WHERE date(minute, 'localtime') < ?
                   AND date(minute, 'localtime') IN
                       (SELECT day FROM RollupProgress WHERE tier = 'daily')
                 LIMIT ?
             )",
        )
        .bind(&cutoff)
        .bind(PRUNE_BATCH)
        .execute(pool)
        .await?
        .rows_affected();
        removed += n;
        if n < PRUNE_BATCH as u64 {
            break;
        }
    }
    Ok(removed)
}

async fn prune_raw_series(
    pool: &SqlitePool,
    table: &str,
    max_batches: usize,
) -> anyhow::Result<u64> {
    // The tier whose coverage gates deletion. Energy's raw rows are the minute
    // tier's source; EnergyStorage's are the storage tier's.
    let tier = if table == "Energy" {
        "minute"
    } else {
        "storage"
    };
    // Retention counts today, so the oldest day kept is today - (N - 1) and
    // anything strictly before that goes.
    let cutoff = (chrono::Local::now().date_naive()
        - chrono::Duration::days(RAW_ENERGY_RETENTION_DAYS - 1))
    .format("%Y-%m-%d")
    .to_string();

    let sql = format!(
        "DELETE FROM {table} WHERE rowid IN (
             SELECT rowid FROM {table}
             WHERE resolution = '2s'
               AND date(timestamp, 'localtime') < ?
               AND date(timestamp, 'localtime') IN
                   (SELECT day FROM RollupProgress WHERE tier = ?)
             LIMIT ?
         )"
    );

    let mut removed = 0u64;
    for _ in 0..max_batches {
        let n = sqlx::query(&sql)
            .bind(&cutoff)
            .bind(tier)
            .bind(PRUNE_BATCH)
            .execute(pool)
            .await?
            .rows_affected();
        removed += n;
        if n < PRUNE_BATCH as u64 {
            break;
        }
    }
    Ok(removed)
}

/// Pages returned to the operating system per pass.
///
/// `auto_vacuum=INCREMENTAL` puts freed pages on a list but never shrinks the
/// file on its own; only `incremental_vacuum` hands them back. Without this the
/// database grows to its high-water mark and stays there — 1.29 GB against 265 MB
/// of live data, before a manual `VACUUM` reclaimed it.
///
/// Bounded per pass because this is the alternative to `VACUUM`, not a smaller
/// version of it: `VACUUM` rewrites the whole file under an exclusive lock and
/// needs Dom stopped, while this moves a few pages at a time and runs alongside
/// everything else. A thousand 4 KB pages is 4 MB an hour, which outpaces
/// anything pruning frees.
const VACUUM_PAGES_PER_PASS: u32 = 1000;

/// Hands a bounded number of freed pages back to the filesystem.
///
/// Returns how many pages remain on the free list, so a caller can see whether it
/// is keeping up.
pub async fn reclaim_free_pages(pool: &SqlitePool) -> anyhow::Result<i64> {
    sqlx::query(&format!(
        "PRAGMA incremental_vacuum({VACUUM_PAGES_PER_PASS})"
    ))
    .execute(pool)
    .await?;
    Ok(sqlx::query_scalar("PRAGMA freelist_count")
        .fetch_one(pool)
        .await
        .unwrap_or(0))
}

pub async fn prune_raw_measurements(pool: &SqlitePool) -> anyhow::Result<u64> {
    let result = sqlx::query(
        "DELETE FROM RawDeviceMeasurements WHERE timestamp < datetime('now', '-1 day')",
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Registers a discovered device, identified by `(device_type, ip)` as
/// before, but reconciled against `fingerprint` (e.g. a MAC address) when
/// one is known. If a device of the same type with the same fingerprint
/// already exists at a *different* ip, that device has moved — this moves
/// its row to the new `ip` in place (preserving its id, credentials, label
/// and history) instead of the new address being registered as an
/// unrelated, second device. Otherwise this behaves like a plain
/// upsert-by-ip, same as before fingerprinting existed.
pub async fn upsert_device(
    pool: &SqlitePool,
    device_type: &str,
    name: &str,
    ip: IpAddr,
    poll_interval_secs: i64,
    fingerprint: Option<&str>,
) -> anyhow::Result<()> {
    let ip_str = ip.to_string();

    if let Some(fp) = fingerprint {
        let existing: Option<(i64, String)> =
            sqlx::query_as("SELECT id, ip FROM Devices WHERE type = ? AND fingerprint = ?")
                .bind(device_type)
                .bind(fp)
                .fetch_optional(pool)
                .await?;
        if let Some((existing_id, existing_ip)) = existing
            && existing_ip != ip_str
        {
            migrate_device_ip(pool, existing_id, &ip_str).await?;
        }
    }

    sqlx::query(
        "INSERT INTO Devices (type, name, ip, poll_interval_secs, fingerprint)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(ip) DO UPDATE SET
             name               = excluded.name,
             poll_interval_secs = excluded.poll_interval_secs,
             fingerprint         = COALESCE(excluded.fingerprint, fingerprint)",
    )
    .bind(device_type)
    .bind(name)
    .bind(&ip_str)
    .bind(poll_interval_secs)
    .bind(fingerprint)
    .execute(pool)
    .await?;
    Ok(())
}

/// Moves every row that belongs to `loser_id` onto `winner_id`.
///
/// Eight tables carry `device_id REFERENCES Devices(id)`, and every one of them
/// has to be moved before the loser row can be deleted — `sqlx` turns on
/// `PRAGMA foreign_keys`, so a table left behind aborts the whole migration with
/// a constraint violation rather than merely orphaning rows. This list having
/// fallen out of date with the schema is exactly how that happened once: the
/// four rollup tiers were added later, and the merge kept naming only the
/// original four.
///
/// The two groups are handled differently because their keys differ.
///
/// The raw series and the timers key on nothing but their own row id, so a plain
/// `UPDATE` moves them and no two rows can collide.
///
/// The four rolled-up tiers all have `device_id` inside a composite primary key,
/// so the same (day, metric) can exist on both sides — the transition day
/// especially, when one address stopped answering and the other started. Moving
/// those needs an aggregate rather than an overwrite, and each tier stores enough
/// to do it exactly:
///
/// - energy is additive, so `EnergyDaily` and `EnergyMinute` sum. `EnergyMinute`
///   keeps its positive and negative parts separate for the reason given at its
///   schema, and they sum independently; `span_secs` sums as covered time; and
///   `peak_w` takes the larger, since a peak is the largest sample either side
///   saw and cannot be recovered from a sum.
/// - `StorageDaily` and `TemperatureDaily` are summaries of a state rather than
///   a flow, so their extremes take min and max. Both store the sample count
///   alongside, which makes the mean exactly recoverable: `StorageDaily` weights
///   the two averages by their counts, and `TemperatureDaily` stores a sum rather
///   than a mean and so simply adds.
///
/// Nothing here is lossy, and nothing invents a value: for every column the
/// result is what a single device reporting both streams would have recorded.
async fn merge_device_history(
    tx: &mut sqlx::SqliteConnection,
    winner_id: i64,
    loser_id: i64,
) -> anyhow::Result<()> {
    // Rows with no per-device uniqueness: move them as they are.
    for table in [
        "RawDeviceMeasurements",
        "Energy",
        "EnergyStorage",
        "SwitchTimers",
    ] {
        sqlx::query(&format!(
            "UPDATE {table} SET device_id = ? WHERE device_id = ?"
        ))
        .bind(winner_id)
        .bind(loser_id)
        .execute(&mut *tx)
        .await?;
    }

    // Rolled-up tiers: combine on conflict, then clear the loser's side.
    let merges = [
        "INSERT INTO EnergyDaily (device_id, day, metric, energy_wh)
         SELECT ?, day, metric, energy_wh FROM EnergyDaily WHERE device_id = ?
         ON CONFLICT(device_id, day, metric) DO UPDATE SET
             energy_wh = EnergyDaily.energy_wh + excluded.energy_wh",
        "INSERT INTO EnergyMinute
             (device_id, minute, metric, energy_ws, energy_ws_pos, energy_ws_neg,
              span_secs, peak_w)
         SELECT ?, minute, metric, energy_ws, energy_ws_pos, energy_ws_neg,
                span_secs, peak_w
         FROM EnergyMinute WHERE device_id = ?
         ON CONFLICT(device_id, minute, metric) DO UPDATE SET
             energy_ws     = EnergyMinute.energy_ws     + excluded.energy_ws,
             energy_ws_pos = EnergyMinute.energy_ws_pos + excluded.energy_ws_pos,
             energy_ws_neg = EnergyMinute.energy_ws_neg + excluded.energy_ws_neg,
             span_secs     = EnergyMinute.span_secs     + excluded.span_secs,
             peak_w        = MAX(EnergyMinute.peak_w, excluded.peak_w)",
        "INSERT INTO StorageDaily (device_id, day, rsoc_min, rsoc_max, rsoc_avg, samples)
         SELECT ?, day, rsoc_min, rsoc_max, rsoc_avg, samples
         FROM StorageDaily WHERE device_id = ?
         ON CONFLICT(device_id, day) DO UPDATE SET
             rsoc_min = MIN(StorageDaily.rsoc_min, excluded.rsoc_min),
             rsoc_max = MAX(StorageDaily.rsoc_max, excluded.rsoc_max),
             rsoc_avg = (StorageDaily.rsoc_avg * StorageDaily.samples
                         + excluded.rsoc_avg * excluded.samples)
                        / (StorageDaily.samples + excluded.samples),
             samples  = StorageDaily.samples + excluded.samples",
        "INSERT INTO TemperatureDaily
             (device_id, day, temp_min, temp_max, temp_sum, samples, last_ts)
         SELECT ?, day, temp_min, temp_max, temp_sum, samples, last_ts
         FROM TemperatureDaily WHERE device_id = ?
         ON CONFLICT(device_id, day) DO UPDATE SET
             temp_min = MIN(TemperatureDaily.temp_min, excluded.temp_min),
             temp_max = MAX(TemperatureDaily.temp_max, excluded.temp_max),
             temp_sum = TemperatureDaily.temp_sum + excluded.temp_sum,
             samples  = TemperatureDaily.samples  + excluded.samples,
             last_ts  = MAX(TemperatureDaily.last_ts, excluded.last_ts)",
    ];
    for sql in merges {
        sqlx::query(sql)
            .bind(winner_id)
            .bind(loser_id)
            .execute(&mut *tx)
            .await?;
    }
    for table in [
        "EnergyDaily",
        "EnergyMinute",
        "StorageDaily",
        "TemperatureDaily",
    ] {
        sqlx::query(&format!("DELETE FROM {table} WHERE device_id = ?"))
            .bind(loser_id)
            .execute(&mut *tx)
            .await?;
    }
    Ok(())
}

/// Moves device `id` to `new_ip`. If another row already occupies `new_ip`
/// (e.g. it was auto-discovered as a "new" device before the fingerprint
/// match caught up), that row's measurement/timer history is reassigned onto
/// `id` first and the now-empty row is removed — `id` (with its
/// credentials, label and history) is always the survivor, since it's the
/// one the fingerprint proves is the same physical device.
async fn migrate_device_ip(pool: &SqlitePool, id: i64, new_ip: &str) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;

    let collision: Option<i64> = sqlx::query_scalar("SELECT id FROM Devices WHERE ip = ?")
        .bind(new_ip)
        .fetch_optional(&mut *tx)
        .await?;

    if let Some(loser_id) = collision {
        merge_device_history(&mut tx, id, loser_id).await?;
        sqlx::query("DELETE FROM Devices WHERE id = ?")
            .bind(loser_id)
            .execute(&mut *tx)
            .await?;
    }

    sqlx::query("UPDATE Devices SET ip = ? WHERE id = ?")
        .bind(new_ip)
        .bind(id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(())
}

/// True if device `id`'s address in the DB no longer matches `ip` (or the
/// row is gone entirely) — i.e. a fingerprint match moved this row out from
/// under a poll loop still bound to the old address. Poll loops check this
/// once they've gone `Lost`, so a stale loop chasing an address the device
/// left behind exits on its own rather than failing forever; a fresh loop
/// for the new address is already running, spawned by the next discovery
/// cycle.
pub async fn device_moved(pool: &SqlitePool, id: i64, ip: IpAddr) -> anyhow::Result<bool> {
    let current: Option<String> = sqlx::query_scalar("SELECT ip FROM Devices WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(current != Some(ip.to_string()))
}

// ── Clock sanity ──────────────────────────────────────────────────────────────

/// The newest instant anything was recorded at.
///
/// Proof that the clock once read at least this — which is the only evidence
/// available, on a machine with no battery-backed clock, that the current time is
/// not nonsense. See `main::await_plausible_clock`.
pub async fn newest_recorded_time(
    pool: &SqlitePool,
) -> anyhow::Result<Option<chrono::DateTime<chrono::Utc>>> {
    let newest: Option<String> = sqlx::query_scalar(
        "SELECT MAX(t) FROM (
             SELECT MAX(timestamp) AS t FROM Energy
             UNION ALL SELECT MAX(timestamp) FROM RawDeviceMeasurements
             UNION ALL SELECT MAX(minute)    FROM EnergyMinute
         )",
    )
    .fetch_optional(pool)
    .await?
    .flatten();
    Ok(newest.as_deref().and_then(parse_timestamp))
}

// ── Battery energy provenance ─────────────────────────────────────────────────

/// `Config` key holding how much of a battery's stored energy came from the grid.
fn grid_origin_key(device_id: i64) -> String {
    format!("battery_grid_origin_wh_{device_id}")
}

/// Reads the battery's stored grid share, or zero if it has never been recorded.
///
/// Zero is the right default rather than an error: a battery whose history is
/// unknown is assumed to hold solar, and the first time it runs flat the figure
/// is corrected against a hard observation anyway — see `energy::GridOrigin`.
pub async fn get_grid_origin_wh(pool: &SqlitePool, device_id: i64) -> anyhow::Result<f64> {
    Ok(get_config(pool, &grid_origin_key(device_id))
        .await?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0))
}

/// Records the battery's stored grid share.
pub async fn set_grid_origin_wh(
    pool: &SqlitePool,
    device_id: i64,
    grid_wh: f64,
) -> anyhow::Result<()> {
    set_config(pool, &grid_origin_key(device_id), &grid_wh.to_string()).await
}

// ── Pinned device certificates ────────────────────────────────────────────────

/// The TLS certificate fingerprint pinned for a device, if one has been.
pub async fn get_tls_pin(pool: &SqlitePool, ip: IpAddr) -> anyhow::Result<Option<String>> {
    Ok(
        sqlx::query_scalar("SELECT tls_fingerprint FROM Devices WHERE ip = ?")
            .bind(ip.to_string())
            .fetch_optional(pool)
            .await?
            .flatten(),
    )
}

/// Pins a device's TLS certificate, replacing any previous pin.
///
/// Called on first contact, and again when a person accepts a changed
/// certificate. Nothing else may call it: a pin that can be rewritten by the
/// code that failed to match it is not a pin.
pub async fn set_tls_pin(pool: &SqlitePool, ip: IpAddr, fingerprint: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE Devices SET tls_fingerprint = ? WHERE ip = ?")
        .bind(fingerprint)
        .bind(ip.to_string())
        .execute(pool)
        .await?;
    Ok(())
}

// ── Site electrical supply ────────────────────────────────────────────────────

/// `Config` keys the site's supply is stored under. Individual settings so a
/// value can be read or corrected by hand, like `LOCATION_KEYS`.
const SUPPLY_KEYS: [&str; 2] = ["supply_volts", "supply_phases"];

/// Reads the site's electrical supply, falling back to the default for anything
/// missing or unreadable.
///
/// Never fails and never returns something implausible: Eco mode divides by this
/// on every tick, and a supply that is absent, half-written or mistyped must not
/// be able to stop the wallbox being controlled — nor to silently mis-scale it.
/// A value that cannot describe a building is logged and ignored.
pub async fn get_supply(pool: &SqlitePool) -> crate::devices::keba::Supply {
    let read = |key: &'static str| async move {
        get_config(pool, key)
            .await
            .ok()
            .flatten()
            .and_then(|v| v.parse::<f64>().ok())
    };
    let default = crate::devices::keba::Supply::default();
    let supply = crate::devices::keba::Supply {
        volts: read(SUPPLY_KEYS[0]).await.unwrap_or(default.volts),
        phases: read(SUPPLY_KEYS[1]).await.unwrap_or(default.phases),
    };
    if supply.is_plausible() {
        supply
    } else {
        log::warn!("stored supply {supply:?} is not plausible; using {default:?}");
        default
    }
}

/// Stores the site's electrical supply.
pub async fn set_supply(
    pool: &SqlitePool,
    supply: crate::devices::keba::Supply,
) -> anyhow::Result<()> {
    if !supply.is_plausible() {
        anyhow::bail!("{supply:?} does not describe a real supply");
    }
    set_config(pool, SUPPLY_KEYS[0], &supply.volts.to_string()).await?;
    set_config(pool, SUPPLY_KEYS[1], &supply.phases.to_string()).await?;
    Ok(())
}

// ── Solar forecast and calibration ────────────────────────────────────────────

/// `Config` keys the fitted array response is stored under. Individual settings
/// rather than one blob, so a value can be read or corrected by hand — the same
/// reasoning as `LOCATION_KEYS`.
const CALIBRATION_KEYS: [&str; 8] = [
    "solar_k",
    "solar_tilt_deg",
    "solar_azimuth_deg",
    "solar_days",
    "solar_samples",
    "solar_rmse_w",
    "solar_daily_rmse_kwh",
    "solar_fitted_at",
];

/// Records forecast steps, without ever rewriting one whose time has passed.
///
/// `cutoff` is the instant that divides them: at or after it a row is still a
/// prediction and is replaced with the fresher one; before it the row already
/// stands as what was expected, and is left exactly as it was. Passing `now` is
/// the normal call. Nothing else enforces this — it is a plain `WHERE` in the
/// upsert — because the guarantee has to hold even if a caller forgets.
pub async fn insert_forecast(
    pool: &SqlitePool,
    points: &[crate::solar::ForecastPoint],
    issued_at: chrono::DateTime<chrono::Utc>,
    cutoff: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<u64> {
    let issued = crate::devices::ts(issued_at);
    let cutoff = crate::devices::ts(cutoff);
    let mut written = 0;
    for p in points {
        let valid_at = crate::devices::ts(p.valid_at);
        if valid_at < cutoff {
            continue;
        }
        let r = sqlx::query(
            "INSERT INTO SolarForecast
                 (valid_at, gti_w_m2, temperature_c, cloud_cover_pct,
                  precipitation_mm, issued_at)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(valid_at) DO UPDATE SET
                 gti_w_m2         = excluded.gti_w_m2,
                 temperature_c    = excluded.temperature_c,
                 cloud_cover_pct  = excluded.cloud_cover_pct,
                 precipitation_mm = excluded.precipitation_mm,
                 issued_at        = excluded.issued_at
             WHERE SolarForecast.valid_at >= ?",
        )
        .bind(&valid_at)
        .bind(p.gti_w_m2)
        .bind(p.temperature_c)
        .bind(p.cloud_cover_pct)
        .bind(p.precipitation_mm)
        .bind(&issued)
        .bind(&cutoff)
        .execute(pool)
        .await?;
        written += r.rows_affected();
    }
    Ok(written)
}

/// Forecast steps covering the local days `from..=to`, in time order.
pub async fn query_forecast(
    pool: &SqlitePool,
    from: chrono::NaiveDate,
    to: chrono::NaiveDate,
) -> anyhow::Result<Vec<crate::solar::ForecastPoint>> {
    let (Some((start, _)), Some((_, end))) = (local_day_bounds_utc(from), local_day_bounds_utc(to))
    else {
        return Ok(Vec::new());
    };
    let rows = sqlx::query(
        "SELECT valid_at, gti_w_m2, temperature_c, cloud_cover_pct, precipitation_mm
         FROM SolarForecast
         WHERE valid_at >= ? AND valid_at < ?
         ORDER BY valid_at",
    )
    .bind(&start)
    .bind(&end)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|row| {
            Some(crate::solar::ForecastPoint {
                valid_at: parse_timestamp(row.get("valid_at"))?,
                gti_w_m2: row.get("gti_w_m2"),
                temperature_c: row.get("temperature_c"),
                cloud_cover_pct: row.get("cloud_cover_pct"),
                precipitation_mm: row.get("precipitation_mm"),
            })
        })
        .collect())
}

/// Parses a stored UTC timestamp. A row whose timestamp cannot be read is
/// dropped by the callers rather than defaulted — an unparseable instant placed
/// at the epoch would silently land in the wrong day.
pub fn parse_timestamp(raw: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::NaiveDateTime::parse_from_str(raw, crate::devices::DB_TIMESTAMP_FMT)
        .ok()
        .map(|n| n.and_utc())
}

/// Fraction of a day that must carry samples before it may be fitted against.
///
/// A day with a poll-loop gap is missing production that the weather still
/// predicts, so fitting on it drags `k` down by however much was missed. This
/// screens the day out entirely rather than trying to repair it: the gap cannot
/// be located precisely enough to subtract, and there are plenty of whole days.
const MIN_DAY_COVERAGE: f64 = 0.98;

/// Local days between `from` and `to` whose production record is complete enough
/// to calibrate against, oldest first.
///
/// `span_secs` counts seconds that actually carried samples — the rollup writes
/// `COUNT(*) * 2`, not a nominal 60 — so summing it over a day and comparing
/// against 86400 is a direct measure of how much of the day was recorded.
pub async fn query_complete_production_days(
    pool: &SqlitePool,
    from: chrono::NaiveDate,
    to: chrono::NaiveDate,
) -> anyhow::Result<Vec<chrono::NaiveDate>> {
    let (Some((start, _)), Some((_, end))) = (local_day_bounds_utc(from), local_day_bounds_utc(to))
    else {
        return Ok(Vec::new());
    };
    let rows: Vec<(String, f64)> = sqlx::query_as(
        // `* 1.0` forces REAL: SUM over an INTEGER column yields INTEGER, and
        // decoding that as f64 is an error rather than a widening.
        "SELECT date(minute, 'localtime') AS day, SUM(span_secs) * 1.0 AS covered
         FROM EnergyMinute
         WHERE metric = 'production' AND minute >= ? AND minute < ?
         GROUP BY day
         HAVING covered >= ?
         ORDER BY day",
    )
    .bind(&start)
    .bind(&end)
    .bind(MIN_DAY_COVERAGE * 86_400.0)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|(day, _)| chrono::NaiveDate::parse_from_str(&day, "%Y-%m-%d").ok())
        .collect())
}

/// Seconds of samples a 15-minute step must carry to be fitted against, out of
/// the 900 a complete one has.
///
/// The screen that matters, and it is finer than the per-day one above. A step
/// that was only half recorded reports half the production the weather predicts,
/// and fitting on it drags the array's scale down by exactly that much. It also
/// catches the whole of a historical data fault: before the integration gap was
/// clamped (see `sonnen_batterie::MAX_INTEGRATION_GAP_MS`) a stalled poll loop
/// wrote hours of energy into one row, and every such row sits in a minute with
/// `span_secs = 2` — so the step containing it falls short here and is dropped,
/// smeared energy and all, without needing to guess a plausible power for an
/// array that has not been measured yet.
const MIN_STEP_COVERAGE_SECS: f64 = 850.0;

/// Measured production in watt-hours per 15-minute step, over the given local
/// days, keyed by the step's start instant in UTC.
///
/// Aggregated from the per-minute tier rather than the 2s series, which is only
/// kept for a few days — so a calibration window of ninety days is readable.
pub async fn query_production_15min(
    pool: &SqlitePool,
    days: &[chrono::NaiveDate],
) -> anyhow::Result<std::collections::HashMap<chrono::DateTime<chrono::Utc>, f64>> {
    let mut out = std::collections::HashMap::new();
    for day in days {
        let Some((start, end)) = local_day_bounds_utc(*day) else {
            continue;
        };
        // Truncate each minute to its quarter-hour: the forecast's step start is
        // what the join is keyed on, and the two must agree exactly.
        let rows: Vec<(String, f64)> = sqlx::query_as(
            "SELECT strftime('%Y-%m-%d %H:', minute)
                    || substr('00' || (CAST(strftime('%M', minute) AS INTEGER) / 15 * 15), -2, 2)
                    || ':00' AS step,
                    SUM(energy_ws) / 3600.0 AS wh
             FROM EnergyMinute
             WHERE metric = 'production' AND minute >= ? AND minute < ?
             GROUP BY step
             HAVING SUM(span_secs) >= ?",
        )
        .bind(&start)
        .bind(&end)
        .bind(MIN_STEP_COVERAGE_SECS)
        .fetch_all(pool)
        .await?;
        for (step, wh) in rows {
            if let Some(at) = parse_timestamp(&step) {
                out.insert(at, wh);
            }
        }
    }
    Ok(out)
}

/// Average power, in W, that `device_id` reported for `metric`, bucketed by
/// quarter-hour-of-day in local time (index 0..`mystrom_switch::STEPS_PER_DAY`,
/// 15-minute steps from local midnight) and averaged over the `days` local
/// days before `today` — `today` itself is excluded, since it is not yet a
/// complete day to average.
///
/// Used by Eco mode (`devices::mystrom_switch::choose_window`) to build both
/// sides of its scoring: the switch's own typical draw, and — subtracted from
/// whole-house consumption by the caller — the household's typical *other*
/// load. A quarter-hour with no samples in the window is simply absent from
/// the map rather than defaulted to zero, so the caller can tell "never
/// measured" apart from "measured at zero".
pub async fn query_avg_power_by_local_quarter_hour(
    pool: &SqlitePool,
    device_id: i64,
    metric: &str,
    today: chrono::NaiveDate,
    days: i64,
) -> anyhow::Result<std::collections::HashMap<usize, f64>> {
    let Some(from) = today.checked_sub_signed(chrono::Duration::days(days)) else {
        return Ok(std::collections::HashMap::new());
    };
    let (Some((from_start, _)), Some((today_start, _))) =
        (local_day_bounds_utc(from), local_day_bounds_utc(today))
    else {
        return Ok(std::collections::HashMap::new());
    };
    let rows: Vec<(i64, f64, f64)> = sqlx::query_as(
        "SELECT
             CAST(strftime('%H', minute, 'localtime') AS INTEGER) * 4
                 + CAST(strftime('%M', minute, 'localtime') AS INTEGER) / 15 AS step,
             SUM(energy_ws) AS total_ws,
             SUM(span_secs) AS total_secs
         FROM EnergyMinute
         WHERE device_id = ? AND metric = ?
           AND minute >= ? AND minute < ?
         GROUP BY step
         HAVING total_secs > 0",
    )
    .bind(device_id)
    .bind(metric)
    .bind(&from_start)
    .bind(&today_start)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .filter(|(step, ..)| *step >= 0)
        .map(|(step, total_ws, total_secs)| (step as usize, total_ws / total_secs))
        .collect())
}

/// Production in kWh for each local day in `from..=to` that has any, from the
/// daily rollup.
pub async fn query_daily_production_kwh(
    pool: &SqlitePool,
    from: chrono::NaiveDate,
    to: chrono::NaiveDate,
) -> anyhow::Result<std::collections::BTreeMap<chrono::NaiveDate, f64>> {
    let rows: Vec<(String, f64)> = sqlx::query_as(
        "SELECT day, SUM(energy_wh) / 1000.0
         FROM EnergyDaily
         WHERE metric = 'production' AND day >= ? AND day <= ?
         GROUP BY day",
    )
    .bind(from.to_string())
    .bind(to.to_string())
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(day, kwh)| {
            Some((
                chrono::NaiveDate::parse_from_str(&day, "%Y-%m-%d").ok()?,
                kwh,
            ))
        })
        .collect())
}

/// Stores a fitted calibration, replacing any previous one.
pub async fn set_calibration(
    pool: &SqlitePool,
    c: &crate::solar::Calibration,
) -> anyhow::Result<()> {
    let values = [
        c.k.to_string(),
        c.tilt_deg.to_string(),
        c.azimuth_deg.to_string(),
        c.days.to_string(),
        c.samples.to_string(),
        c.rmse_w.to_string(),
        c.daily_rmse_kwh.to_string(),
        crate::devices::ts(c.fitted_at),
    ];
    for (key, value) in CALIBRATION_KEYS.iter().zip(values.iter()) {
        set_config(pool, key, value).await?;
    }
    Ok(())
}

/// Reads the stored calibration, or `None` if the array has never been fitted.
///
/// A partially written or hand-edited set of keys reads as `None` rather than as
/// a calibration with defaults filled in: predicting against half a fit would be
/// worse than predicting nothing, and visibly so.
pub async fn get_calibration(
    pool: &SqlitePool,
) -> anyhow::Result<Option<crate::solar::Calibration>> {
    let mut values = Vec::with_capacity(CALIBRATION_KEYS.len());
    for key in CALIBRATION_KEYS {
        match get_config(pool, key).await? {
            Some(v) => values.push(v),
            None => return Ok(None),
        }
    }
    let (Ok(k), Ok(tilt_deg), Ok(azimuth_deg), Ok(days), Ok(samples), Ok(rmse_w), Ok(daily)) = (
        values[0].parse(),
        values[1].parse(),
        values[2].parse(),
        values[3].parse(),
        values[4].parse(),
        values[5].parse(),
        values[6].parse(),
    ) else {
        return Ok(None);
    };
    let Some(fitted_at) = parse_timestamp(&values[7]) else {
        return Ok(None);
    };
    Ok(Some(crate::solar::Calibration {
        k,
        tilt_deg,
        azimuth_deg,
        days,
        samples,
        rmse_w,
        daily_rmse_kwh: daily,
        fitted_at,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    // These exercise the raw (non-macro) SQL in the new network-history/traffic
    // queries against a real in-memory schema, since sqlx::query isn't
    // compile-time checked the way sqlx::query! would be.

    // ── Daily energy rollup ───────────────────────────────────────────────────

    use chrono::NaiveDate;

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    /// Inserts a 2s Energy sample at a given *local* wall-clock time.
    async fn sample(pool: &SqlitePool, device_id: i64, local: &str, metric: &str, ws: f64) {
        use chrono::{Local, TimeZone};
        let naive = chrono::NaiveDateTime::parse_from_str(local, "%Y-%m-%d %H:%M:%S").unwrap();
        let utc = Local
            .from_local_datetime(&naive)
            .earliest()
            .unwrap()
            .naive_utc();
        sqlx::query(
            "INSERT INTO Energy (device_id, timestamp, resolution, metric, energy_ws)
             VALUES (?, ?, '2s', ?, ?)",
        )
        .bind(device_id)
        .bind(utc.format(crate::devices::DB_TIMESTAMP_FMT).to_string())
        .bind(metric)
        .bind(ws)
        .execute(pool)
        .await
        .unwrap();
    }

    /// Rolls one device-day through the tiers in the order `rollup_history` uses.
    /// The daily tier reads the minute tier, so the two cannot be run out of order.
    async fn roll_day(pool: &SqlitePool, id: i64, d: NaiveDate) {
        rollup_energy_minute(pool, id, d, NO_CUTOFF).await.unwrap();
        rollup_energy_day(pool, id, d).await.unwrap();
    }

    async fn device(pool: &SqlitePool, ip: &str) -> i64 {
        upsert_device(pool, "sonnen_batterie", "b", ip.parse().unwrap(), 2, None)
            .await
            .unwrap();
        sqlx::query_scalar("SELECT id FROM Devices WHERE ip = ?")
            .bind(ip)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn local_day_bounds_span_exactly_one_local_day() {
        let (start, end) = local_day_bounds_utc(day("2026-03-15")).unwrap();
        assert!(start < end);
        // Whatever the offset, consecutive days must abut exactly: one day's end
        // is the next day's start, so no sample can fall in a gap or be counted
        // twice.
        let (next_start, _) = local_day_bounds_utc(day("2026-03-16")).unwrap();
        assert_eq!(end, next_start);
    }

    #[tokio::test]
    async fn rollup_on_an_empty_database_does_nothing_and_does_not_error() {
        // The first-launch path: no Energy rows at all. SELECT MIN() over an
        // empty table yields one NULL row, which must read as "nothing to do"
        // rather than failing to decode.
        let pool = init("sqlite::memory:").await.unwrap();
        let written = rollup_history(&pool, std::time::Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(written, 0);
        assert_eq!(oldest_energy_day(&pool).await.unwrap(), None);
        assert!(
            query_daily_energy(&pool, day("2026-01-01"), day("2026-12-31"))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn rollup_with_devices_but_no_energy_rows_writes_nothing() {
        let pool = init("sqlite::memory:").await.unwrap();
        let _ = device(&pool, "10.0.0.9").await;
        assert_eq!(
            rollup_history(&pool, std::time::Duration::ZERO)
                .await
                .unwrap(),
            0
        );
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM EnergyDaily")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 0);
    }

    // ── Minute and storage tiers ──────────────────────────────────────────────

    /// A cutoff far in the future, so tests roll up every minute they seeded.
    const NO_CUTOFF: &str = "2099-01-01 00:00:00";

    async fn storage_sample(pool: &SqlitePool, device_id: i64, local: &str, rsoc: f64) {
        use chrono::{Local, TimeZone};
        let naive = chrono::NaiveDateTime::parse_from_str(local, "%Y-%m-%d %H:%M:%S").unwrap();
        let utc = Local
            .from_local_datetime(&naive)
            .earliest()
            .unwrap()
            .naive_utc();
        sqlx::query(
            "INSERT INTO EnergyStorage (device_id, timestamp, resolution, rsoc_avg)
             VALUES (?, ?, '2s', ?)",
        )
        .bind(device_id)
        .bind(utc.format(crate::devices::DB_TIMESTAMP_FMT).to_string())
        .bind(rsoc)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn minute_rollup_preserves_energy_exactly_and_splits_by_sign() {
        let pool = init("sqlite::memory:").await.unwrap();
        let id = device(&pool, "10.1.0.1").await;
        // Three samples inside one minute, two directions.
        sample(&pool, id, "2026-05-10 09:00:00", "grid", -3600.0).await;
        sample(&pool, id, "2026-05-10 09:00:02", "grid", -1800.0).await;
        sample(&pool, id, "2026-05-10 09:00:04", "grid", 7200.0).await;

        rollup_energy_minute(&pool, id, day("2026-05-10"), NO_CUTOFF)
            .await
            .unwrap();

        let row = sqlx::query(
            "SELECT energy_ws, energy_ws_pos, energy_ws_neg, span_secs, peak_w
             FROM EnergyMinute WHERE device_id = ? AND metric = 'grid'",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let net: f64 = row.get("energy_ws");
        let pos: f64 = row.get("energy_ws_pos");
        let neg: f64 = row.get("energy_ws_neg");
        let span: i64 = row.get("span_secs");
        let peak: f64 = row.get("peak_w");

        // Net is the exact integral: -3600 - 1800 + 7200.
        assert!((net - 1800.0).abs() < 1e-9, "{net}");
        // Both directions survive. Splitting after aggregation would have given
        // 1800 exported and nothing imported, losing 5400 Ws of import.
        assert!((pos - 7200.0).abs() < 1e-9, "{pos}");
        assert!((neg - 5400.0).abs() < 1e-9, "{neg}");
        assert_eq!(span, 6, "three 2s samples cover six seconds, not sixty");
        // Largest 2s average power in the minute: 7200 Ws over 2 s.
        assert!((peak - 3600.0).abs() < 1e-9, "{peak}");
    }

    #[tokio::test]
    async fn minute_rollup_buckets_by_minute() {
        let pool = init("sqlite::memory:").await.unwrap();
        let id = device(&pool, "10.1.0.2").await;
        sample(&pool, id, "2026-05-10 09:00:58", "consumption", 3600.0).await;
        sample(&pool, id, "2026-05-10 09:01:00", "consumption", 7200.0).await;

        rollup_energy_minute(&pool, id, day("2026-05-10"), NO_CUTOFF)
            .await
            .unwrap();

        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM EnergyMinute")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            rows, 2,
            "samples either side of :00 belong to different minutes"
        );
    }

    #[tokio::test]
    async fn minute_rollup_skips_the_minute_still_in_progress() {
        let pool = init("sqlite::memory:").await.unwrap();
        let id = device(&pool, "10.1.0.3").await;
        sample(&pool, id, "2026-05-10 09:00:00", "consumption", 3600.0).await;
        sample(&pool, id, "2026-05-10 09:01:00", "consumption", 3600.0).await;

        // Pretend "now" is inside 09:01, so only 09:00 is complete. Writing a
        // partial 09:01 row would look final once the 2s rows were pruned.
        let cutoff = {
            use chrono::{Local, TimeZone};
            let naive =
                chrono::NaiveDateTime::parse_from_str("2026-05-10 09:01:00", "%Y-%m-%d %H:%M:%S")
                    .unwrap();
            Local
                .from_local_datetime(&naive)
                .earliest()
                .unwrap()
                .naive_utc()
                .format(crate::devices::DB_TIMESTAMP_FMT)
                .to_string()
        };
        rollup_energy_minute(&pool, id, day("2026-05-10"), &cutoff)
            .await
            .unwrap();

        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM EnergyMinute")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 1, "only the completed minute is written");
    }

    #[tokio::test]
    async fn minute_rollup_is_idempotent() {
        let pool = init("sqlite::memory:").await.unwrap();
        let id = device(&pool, "10.1.0.4").await;
        sample(&pool, id, "2026-05-10 09:00:00", "consumption", 3600.0).await;

        for _ in 0..3 {
            rollup_energy_minute(&pool, id, day("2026-05-10"), NO_CUTOFF)
                .await
                .unwrap();
        }
        let (rows, total): (i64, f64) =
            sqlx::query_as("SELECT COUNT(*), SUM(energy_ws) FROM EnergyMinute")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(rows, 1);
        assert!(
            (total - 3600.0).abs() < 1e-9,
            "replaced, not accumulated: {total}"
        );
    }

    #[tokio::test]
    async fn minute_tier_totals_match_the_daily_tier_exactly() {
        // The gate for pruning the 2s series: rolling a day up by minutes and
        // then summing those minutes must equal rolling the same day up directly.
        // Summing an integral in two stages is lossless, and this proves it on
        // real arithmetic rather than by assertion.
        let pool = init("sqlite::memory:").await.unwrap();
        let id = device(&pool, "10.1.0.5").await;
        for (i, metric) in ["consumption", "production", "grid"].iter().enumerate() {
            for m in 0..90 {
                let ws = match *metric {
                    "grid" if m % 3 == 0 => -1234.5 - m as f64,
                    "grid" => 987.6 + m as f64,
                    _ => 1000.0 + (m as f64) * (i as f64 + 1.0),
                };
                sample(
                    &pool,
                    id,
                    &format!(
                        "2026-05-10 {:02}:{:02}:{:02}",
                        8 + m / 60,
                        m % 60,
                        (m * 7) % 60
                    ),
                    metric,
                    ws,
                )
                .await;
            }
        }

        rollup_energy_minute(&pool, id, day("2026-05-10"), NO_CUTOFF)
            .await
            .unwrap();
        roll_day(&pool, id, day("2026-05-10")).await;

        // From the daily tier.
        let daily = query_daily_energy(&pool, day("2026-05-10"), day("2026-05-10"))
            .await
            .unwrap();
        let d = &daily[0];

        // The same figures rebuilt from the minute tier.
        let row = sqlx::query(
            "SELECT
                 SUM(CASE WHEN metric='consumption' THEN energy_ws     ELSE 0 END)/3600.0/1000.0 AS c,
                 SUM(CASE WHEN metric='production'  THEN energy_ws     ELSE 0 END)/3600.0/1000.0 AS p,
                 SUM(CASE WHEN metric='grid'        THEN energy_ws_neg ELSE 0 END)/3600.0/1000.0 AS imp,
                 SUM(CASE WHEN metric='grid'        THEN energy_ws_pos ELSE 0 END)/3600.0/1000.0 AS exp
             FROM EnergyMinute WHERE device_id = ?",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let (c, p, imp, exp): (f64, f64, f64, f64) =
            (row.get("c"), row.get("p"), row.get("imp"), row.get("exp"));

        assert!(
            (c - d.consumption_kwh).abs() < 1e-9,
            "consumption {c} vs {:?}",
            d
        );
        assert!(
            (p - d.production_kwh).abs() < 1e-9,
            "production {p} vs {:?}",
            d
        );
        assert!(
            (imp - d.grid_import_kwh).abs() < 1e-9,
            "import {imp} vs {:?}",
            d
        );
        assert!(
            (exp - d.grid_export_kwh).abs() < 1e-9,
            "export {exp} vs {:?}",
            d
        );
    }

    #[tokio::test]
    async fn storage_rollup_summarises_state_of_charge() {
        let pool = init("sqlite::memory:").await.unwrap();
        let id = device(&pool, "10.1.0.6").await;
        storage_sample(&pool, id, "2026-05-10 06:00:00", 20.0).await;
        storage_sample(&pool, id, "2026-05-10 12:00:00", 90.0).await;
        storage_sample(&pool, id, "2026-05-10 20:00:00", 40.0).await;
        // A different day must not bleed in.
        storage_sample(&pool, id, "2026-05-11 06:00:00", 5.0).await;

        rollup_storage_day(&pool, id, day("2026-05-10"))
            .await
            .unwrap();

        let row = sqlx::query(
            "SELECT rsoc_min, rsoc_max, rsoc_avg, samples FROM StorageDaily WHERE day = '2026-05-10'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let lo: f64 = row.get("rsoc_min");
        let hi: f64 = row.get("rsoc_max");
        let mean: f64 = row.get("rsoc_avg");
        let n: i64 = row.get("samples");
        assert_eq!((lo, hi, n), (20.0, 90.0, 3));
        assert!((mean - 50.0).abs() < 1e-9, "{mean}");
    }

    #[tokio::test]
    async fn storage_rollup_writes_nothing_when_there_are_no_samples() {
        let pool = init("sqlite::memory:").await.unwrap();
        let id = device(&pool, "10.1.0.7").await;
        rollup_storage_day(&pool, id, day("2026-05-10"))
            .await
            .unwrap();
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM StorageDaily")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[tokio::test]
    async fn rollup_history_backfills_a_new_tier_across_days_the_old_one_already_covered() {
        // The migration case: EnergyDaily rows exist from before EnergyMinute
        // did. Those days must still be worked, or their minute rows would never
        // exist and pruning the 2s series would lose them permanently.
        let pool = init("sqlite::memory:").await.unwrap();
        let id = device(&pool, "10.1.0.8").await;
        let today = chrono::Local::now().date_naive();
        for back in 2..6 {
            let d = today - chrono::Duration::days(back);
            sample(
                &pool,
                id,
                &format!("{} 09:00:00", d.format("%Y-%m-%d")),
                "consumption",
                3600.0,
            )
            .await;
            // Pretend the daily tier already covered this day, as it would have
            // before the minute tier existed.
            rollup_energy_minute(&pool, id, d, NO_CUTOFF).await.unwrap();
            rollup_energy_day(&pool, id, d).await.unwrap();
            sqlx::query("DELETE FROM EnergyMinute")
                .execute(&pool)
                .await
                .unwrap();
        }
        let minutes_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM EnergyMinute")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(minutes_before, 0);

        rollup_history(&pool, std::time::Duration::ZERO)
            .await
            .unwrap();

        let minutes_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM EnergyMinute")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(minutes_after, 4, "every day with data gets minute rows");

        // And a second pass settles to just the re-rolled tail.
        let second = rollup_history(&pool, std::time::Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(second, DAYS_ALWAYS_REROLLED);
    }

    // ── Pruning the raw series ────────────────────────────────────────────────

    /// Seeds one 2s sample per day for `days` days back, the newest a couple of
    /// minutes ago. Returns the local days seeded, newest first.
    ///
    /// Anchored to *now* rather than to a fixed hour of the day, and that is not
    /// tidiness. The minute tier only writes minutes that are over, so a sample
    /// stamped 09:00 was in the *future* for any run between local midnight and
    /// nine in the morning: today then produced no minute rows and no daily row,
    /// and the counts came up one short. The suite passed all afternoon and
    /// failed for the nine hours around midnight, which is exactly the shape of
    /// bug that goes unnoticed for months.
    ///
    /// The days come back so a caller can state its expectations against the
    /// same anchor — near midnight the newest seeded day may be yesterday, and
    /// a test that hard-codes "today" would be asserting something else.
    async fn seed_days(
        pool: &SqlitePool,
        id: i64,
        days: i64,
        metric: &str,
    ) -> Vec<chrono::NaiveDate> {
        // Two minutes, not one: the sample's own minute has to be over, and one
        // minute would leave that to a rounding boundary.
        let anchor = chrono::Local::now() - chrono::Duration::minutes(2);
        let mut seeded = Vec::new();
        for back in 0..days {
            let at = anchor - chrono::Duration::days(back);
            sample(
                pool,
                id,
                &at.format("%Y-%m-%d %H:%M:%S").to_string(),
                metric,
                3600.0,
            )
            .await;
            seeded.push(at.date_naive());
        }
        seeded
    }

    async fn count(pool: &SqlitePool, sql: &str) -> i64 {
        sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
    }

    #[tokio::test]
    async fn pruning_refuses_to_touch_days_the_minute_tier_has_not_processed() {
        // The interlock that makes pruning safe. Old raw rows exist, but nothing
        // has been rolled up, so nothing may be deleted — otherwise a rollup
        // failure followed by a prune would destroy the data permanently.
        let pool = init("sqlite::memory:").await.unwrap();
        let id = device(&pool, "10.2.0.1").await;
        seed_days(&pool, id, 20, "consumption").await;
        let before = count(&pool, "SELECT COUNT(*) FROM Energy").await;

        let removed = prune_energy_2s(&pool, 10).await.unwrap();

        assert_eq!(removed, 0, "must not delete unrolled data");
        assert_eq!(count(&pool, "SELECT COUNT(*) FROM Energy").await, before);
    }

    #[tokio::test]
    async fn pruning_removes_rolled_up_days_beyond_the_retention_window() {
        let pool = init("sqlite::memory:").await.unwrap();
        let id = device(&pool, "10.2.0.2").await;
        let seeded = seed_days(&pool, id, 20, "consumption").await;

        rollup_history(&pool, std::time::Duration::ZERO)
            .await
            .unwrap();
        let rolled_total = count(&pool, "SELECT COUNT(*) FROM EnergyMinute").await;
        assert_eq!(rolled_total, 20, "every day rolled up before pruning");

        let removed = prune_energy_2s(&pool, 10).await.unwrap();
        assert!(removed > 0, "old rolled-up days should go");

        // What survives is exactly the seeded days inside the retention window.
        // Counted from the same anchor `seed_days` used rather than assumed to
        // be `RAW_ENERGY_RETENTION_DAYS`: run just after local midnight the
        // newest seeded day is yesterday, and one fewer day falls inside it.
        let oldest_kept = chrono::Local::now().date_naive()
            - chrono::Duration::days(RAW_ENERGY_RETENTION_DAYS - 1);
        let expected = seeded.iter().filter(|d| **d >= oldest_kept).count();
        assert!(expected > 0, "the window has to keep something");
        let remaining = count(&pool, "SELECT COUNT(*) FROM Energy").await;
        assert_eq!(remaining as usize, expected);

        // And nothing was lost from the tiers that have to outlive the raw rows.
        assert_eq!(
            count(&pool, "SELECT COUNT(*) FROM EnergyMinute").await,
            rolled_total
        );
        // One row per metric per day, so check the day coverage rather than rows.
        assert_eq!(
            count(&pool, "SELECT COUNT(DISTINCT day) FROM EnergyDaily").await,
            20
        );
    }

    #[tokio::test]
    async fn pruning_keeps_everything_inside_the_reroll_window() {
        // A day still subject to re-rolling must still have its source rows, or
        // the re-roll would overwrite a complete total with a partial one.
        let pool = init("sqlite::memory:").await.unwrap();
        let id = device(&pool, "10.2.0.3").await;
        seed_days(&pool, id, 20, "consumption").await;
        rollup_history(&pool, std::time::Duration::ZERO)
            .await
            .unwrap();
        prune_energy_2s(&pool, 10).await.unwrap();

        let today = chrono::Local::now().date_naive();
        for back in 0..DAYS_ALWAYS_REROLLED as i64 {
            let d = today - chrono::Duration::days(back);
            let (start, end) = local_day_bounds_utc(d).unwrap();
            let n: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM Energy WHERE timestamp >= ? AND timestamp < ?",
            )
            .bind(&start)
            .bind(&end)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(
                n, 1,
                "{d} is inside the re-roll window and must keep its rows"
            );
        }
    }

    #[tokio::test]
    async fn totals_survive_pruning_unchanged() {
        // The whole point: after the raw rows are gone, the reported history is
        // still exactly what it was.
        let pool = init("sqlite::memory:").await.unwrap();
        let id = device(&pool, "10.2.0.4").await;
        let today = chrono::Local::now().date_naive();
        for back in 0..15 {
            let d = today - chrono::Duration::days(back);
            for (h, metric, ws) in [
                (7, "grid", -3600.0),
                (9, "consumption", 7200.0),
                (12, "production", 18000.0),
                (13, "grid", 10800.0),
            ] {
                sample(
                    &pool,
                    id,
                    &format!("{} {h:02}:00:00", d.format("%Y-%m-%d")),
                    metric,
                    ws,
                )
                .await;
            }
        }
        rollup_history(&pool, std::time::Duration::ZERO)
            .await
            .unwrap();
        let before = query_daily_energy(&pool, today - chrono::Duration::days(14), today)
            .await
            .unwrap();

        let removed = prune_energy_2s(&pool, 20).await.unwrap();
        assert!(removed > 0);
        // Re-roll after pruning: this is where a naive implementation would
        // overwrite good totals with partial ones from the surviving rows.
        rollup_history(&pool, std::time::Duration::ZERO)
            .await
            .unwrap();

        let after = query_daily_energy(&pool, today - chrono::Duration::days(14), today)
            .await
            .unwrap();
        assert_eq!(before, after, "history must be unchanged by pruning");
    }

    #[tokio::test]
    async fn storage_pruning_is_gated_on_its_own_tier() {
        let pool = init("sqlite::memory:").await.unwrap();
        let id = device(&pool, "10.2.0.5").await;
        // Energy rows drive the day enumeration; storage rows are what get pruned.
        seed_days(&pool, id, 20, "consumption").await;
        let today = chrono::Local::now().date_naive();
        for back in 0..20 {
            let d = today - chrono::Duration::days(back);
            storage_sample(
                &pool,
                id,
                &format!("{} 09:00:00", d.format("%Y-%m-%d")),
                55.0,
            )
            .await;
        }

        assert_eq!(
            prune_energy_storage_2s(&pool, 10).await.unwrap(),
            0,
            "nothing rolled up yet"
        );

        rollup_history(&pool, std::time::Duration::ZERO)
            .await
            .unwrap();
        let removed = prune_energy_storage_2s(&pool, 10).await.unwrap();
        assert!(removed > 0);
        assert_eq!(
            count(&pool, "SELECT COUNT(*) FROM EnergyStorage").await,
            RAW_ENERGY_RETENTION_DAYS
        );
        assert_eq!(count(&pool, "SELECT COUNT(*) FROM StorageDaily").await, 20);
    }

    #[tokio::test]
    async fn pruning_respects_its_batch_cap() {
        let pool = init("sqlite::memory:").await.unwrap();
        let id = device(&pool, "10.2.0.6").await;
        seed_days(&pool, id, 20, "consumption").await;
        rollup_history(&pool, std::time::Duration::ZERO)
            .await
            .unwrap();

        // One batch is far larger than this dataset, so a single batch clears the
        // backlog; the point is that max_batches = 0 does nothing at all.
        assert_eq!(prune_energy_2s(&pool, 0).await.unwrap(), 0);
        assert!(prune_energy_2s(&pool, 1).await.unwrap() > 0);
    }

    #[tokio::test]
    async fn minute_pruning_is_gated_on_the_daily_tier_and_respects_its_window() {
        let pool = init("sqlite::memory:").await.unwrap();
        let id = device(&pool, "10.2.0.7").await;
        let today = chrono::Local::now().date_naive();
        // A day inside the minute window and one well outside it.
        for back in [1_i64, MINUTE_RETENTION_DAYS + 10] {
            let d = today - chrono::Duration::days(back);
            sample(
                &pool,
                id,
                &format!("{} 09:00:00", d.format("%Y-%m-%d")),
                "consumption",
                3600.0,
            )
            .await;
        }

        // Minute rows exist but the daily tier has not run: nothing may go.
        rollup_energy_minute(
            &pool,
            id,
            today - chrono::Duration::days(MINUTE_RETENTION_DAYS + 10),
            NO_CUTOFF,
        )
        .await
        .unwrap();
        assert_eq!(prune_energy_minute(&pool, 5).await.unwrap(), 0);

        // With the whole pipeline run, the out-of-window day goes and the recent
        // one stays.
        rollup_history(&pool, std::time::Duration::ZERO)
            .await
            .unwrap();
        let before = count(&pool, "SELECT COUNT(*) FROM EnergyMinute").await;
        let removed = prune_energy_minute(&pool, 5).await.unwrap();
        assert!(removed > 0, "the old day should be dropped");
        assert_eq!(
            count(&pool, "SELECT COUNT(*) FROM EnergyMinute").await,
            before - removed as i64
        );
        // The daily total for the pruned day survives — that is the point.
        let old_day = today - chrono::Duration::days(MINUTE_RETENTION_DAYS + 10);
        let got = query_daily_energy(&pool, old_day, old_day).await.unwrap();
        assert_eq!(got.len(), 1, "daily history must outlive the minute rows");
        assert!((got[0].consumption_kwh - 0.001).abs() < 1e-9);
    }

    // ── Temperature tier ──────────────────────────────────────────────────────

    async fn temp_sample(pool: &SqlitePool, device_id: i64, local: &str, value: f64) {
        use chrono::{Local, TimeZone};
        let naive = chrono::NaiveDateTime::parse_from_str(local, "%Y-%m-%d %H:%M:%S").unwrap();
        let utc = Local
            .from_local_datetime(&naive)
            .earliest()
            .unwrap()
            .naive_utc();
        sqlx::query(
            "INSERT INTO RawDeviceMeasurements (device_id, timestamp, metric, value)
             VALUES (?, ?, 'temperature', ?)",
        )
        .bind(device_id)
        .bind(utc.format(crate::devices::DB_TIMESTAMP_FMT).to_string())
        .bind(value)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn temp_of(pool: &SqlitePool, d: NaiveDate) -> Option<DailyTemperature> {
        query_daily_temperature(pool, d, d).await.unwrap().pop()
    }

    #[tokio::test]
    async fn temperature_rollup_records_min_max_and_average() {
        let pool = init("sqlite::memory:").await.unwrap();
        let id = device(&pool, "10.3.0.1").await;
        for (t, v) in [("06:00:00", 18.0), ("12:00:00", 26.0), ("20:00:00", 22.0)] {
            temp_sample(&pool, id, &format!("2026-05-10 {t}"), v).await;
        }

        rollup_temperature_day(&pool, id, day("2026-05-10"))
            .await
            .unwrap();

        let got = temp_of(&pool, day("2026-05-10")).await.unwrap();
        assert_eq!((got.min_c, got.max_c), (18.0, 26.0));
        assert!((got.avg_c - 22.0).abs() < 1e-9, "{got:?}");
    }

    #[tokio::test]
    async fn temperature_rollup_folds_in_later_samples_without_double_counting() {
        // The behaviour the whole design turns on: the source is pruned after a
        // day, so a day is accumulated as it happens. Re-running must add only
        // what is new.
        let pool = init("sqlite::memory:").await.unwrap();
        let id = device(&pool, "10.3.0.2").await;

        temp_sample(&pool, id, "2026-05-10 06:00:00", 18.0).await;
        temp_sample(&pool, id, "2026-05-10 07:00:00", 20.0).await;
        rollup_temperature_day(&pool, id, day("2026-05-10"))
            .await
            .unwrap();
        let first = temp_of(&pool, day("2026-05-10")).await.unwrap();
        assert_eq!((first.min_c, first.max_c), (18.0, 20.0));
        assert!((first.avg_c - 19.0).abs() < 1e-9);

        // Running again with nothing new must change nothing at all.
        for _ in 0..3 {
            rollup_temperature_day(&pool, id, day("2026-05-10"))
                .await
                .unwrap();
        }
        assert_eq!(temp_of(&pool, day("2026-05-10")).await.unwrap(), first);

        // Now the day continues.
        temp_sample(&pool, id, "2026-05-10 13:00:00", 28.0).await;
        temp_sample(&pool, id, "2026-05-10 22:00:00", 16.0).await;
        rollup_temperature_day(&pool, id, day("2026-05-10"))
            .await
            .unwrap();

        let got = temp_of(&pool, day("2026-05-10")).await.unwrap();
        assert_eq!((got.min_c, got.max_c), (16.0, 28.0), "extremes must widen");
        // Mean of all four samples, not of the last two.
        assert!((got.avg_c - 20.5).abs() < 1e-9, "{got:?}");
    }

    #[tokio::test]
    async fn temperature_rollup_survives_its_source_being_pruned() {
        // The scenario the incremental design exists for: yesterday is folded in
        // while it happens, then its raw samples age out. A later pass must not
        // replace the recorded day with whatever sliver of it still remains.
        let pool = init("sqlite::memory:").await.unwrap();
        let id = device(&pool, "10.3.0.3").await;
        for (t, v) in [("06:00:00", 10.0), ("12:00:00", 30.0), ("23:30:00", 20.0)] {
            temp_sample(&pool, id, &format!("2026-05-10 {t}"), v).await;
        }
        rollup_temperature_day(&pool, id, day("2026-05-10"))
            .await
            .unwrap();
        let complete = temp_of(&pool, day("2026-05-10")).await.unwrap();
        assert_eq!((complete.min_c, complete.max_c), (10.0, 30.0));

        // Everything but the tail of the day is pruned, as the 24h window does.
        sqlx::query("DELETE FROM RawDeviceMeasurements WHERE value IN (10.0, 30.0)")
            .execute(&pool)
            .await
            .unwrap();

        rollup_temperature_day(&pool, id, day("2026-05-10"))
            .await
            .unwrap();

        assert_eq!(
            temp_of(&pool, day("2026-05-10")).await.unwrap(),
            complete,
            "a recorded day must not be degraded by re-running against pruned source data"
        );
    }

    #[tokio::test]
    async fn temperature_rollup_keeps_days_and_devices_apart() {
        let pool = init("sqlite::memory:").await.unwrap();
        let a = device(&pool, "10.3.0.4").await;
        let b = device(&pool, "10.3.0.5").await;
        temp_sample(&pool, a, "2026-05-10 12:00:00", 20.0).await;
        temp_sample(&pool, b, "2026-05-10 12:00:00", 30.0).await;
        temp_sample(&pool, a, "2026-05-11 12:00:00", 25.0).await;

        for d in ["2026-05-10", "2026-05-11"] {
            rollup_temperature_day(&pool, a, day(d)).await.unwrap();
            rollup_temperature_day(&pool, b, day(d)).await.unwrap();
        }

        let d10 = query_daily_temperature(&pool, day("2026-05-10"), day("2026-05-10"))
            .await
            .unwrap();
        assert_eq!(d10.len(), 2, "one row per device");
        let d11 = query_daily_temperature(&pool, day("2026-05-11"), day("2026-05-11"))
            .await
            .unwrap();
        assert_eq!(d11.len(), 1, "only one device reported on the 11th");
        assert_eq!(d11[0].ip.to_string(), "10.3.0.4");
    }

    #[tokio::test]
    async fn temperature_rollup_writes_nothing_without_samples() {
        let pool = init("sqlite::memory:").await.unwrap();
        let id = device(&pool, "10.3.0.6").await;
        rollup_temperature_day(&pool, id, day("2026-05-10"))
            .await
            .unwrap();
        assert_eq!(
            count(&pool, "SELECT COUNT(*) FROM TemperatureDaily").await,
            0
        );
        assert!(temp_of(&pool, day("2026-05-10")).await.is_none());
    }

    #[tokio::test]
    async fn temperature_ignores_other_metrics() {
        let pool = init("sqlite::memory:").await.unwrap();
        let id = device(&pool, "10.3.0.7").await;
        temp_sample(&pool, id, "2026-05-10 12:00:00", 20.0).await;
        sqlx::query(
            "INSERT INTO RawDeviceMeasurements (device_id, timestamp, metric, value)
             VALUES (?, '2026-05-10 10:00:00', 'power', 9999.0)",
        )
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();

        rollup_temperature_day(&pool, id, day("2026-05-10"))
            .await
            .unwrap();

        let got = temp_of(&pool, day("2026-05-10")).await.unwrap();
        assert_eq!(
            (got.min_c, got.max_c),
            (20.0, 20.0),
            "power must not leak in"
        );
    }

    #[tokio::test]
    async fn local_day_bounds_are_converted_out_of_local_time() {
        use chrono::{Local, TimeZone};

        // The bug this guards against is treating the local date as if it were
        // already UTC: the bounds must land on local midnight, which in any
        // non-UTC zone is a different wall-clock time in UTC. Asserted by
        // converting back rather than against a fixed offset, so the test holds
        // in whatever zone it runs.
        let (start, _) = local_day_bounds_utc(day("2026-08-15")).unwrap();
        let start_utc =
            chrono::NaiveDateTime::parse_from_str(&start, crate::devices::DB_TIMESTAMP_FMT)
                .unwrap();
        let back = Local.from_utc_datetime(&start_utc);
        assert_eq!(
            back.format("%Y-%m-%d %H:%M:%S").to_string(),
            "2026-08-15 00:00:00"
        );
    }

    #[tokio::test]
    async fn local_day_bounds_span_a_whole_day_across_a_dst_transition() {
        // In zones that observe it, the spring-forward day is 23 hours and the
        // autumn one 25. Taking the next local midnight rather than start+24h is
        // what keeps a day's samples from spilling into its neighbour.
        for d in ["2026-03-29", "2026-10-25", "2026-06-15"] {
            let (start, end) = local_day_bounds_utc(day(d)).unwrap();
            let s = chrono::NaiveDateTime::parse_from_str(&start, crate::devices::DB_TIMESTAMP_FMT)
                .unwrap();
            let e = chrono::NaiveDateTime::parse_from_str(&end, crate::devices::DB_TIMESTAMP_FMT)
                .unwrap();
            let hours = (e - s).num_hours();
            assert!(
                (23..=25).contains(&hours),
                "{d}: a local day should be 23-25 hours, got {hours}"
            );
        }
    }

    #[tokio::test]
    async fn rollup_sums_a_day_and_splits_grid_by_sign() {
        let pool = init("sqlite::memory:").await.unwrap();
        let id = device(&pool, "10.0.0.1").await;

        // 3600 Ws = 1 Wh, so these are round numbers in Wh.
        sample(&pool, id, "2026-05-10 09:00:00", "consumption", 7200.0).await;
        sample(&pool, id, "2026-05-10 21:00:00", "consumption", 3600.0).await;
        sample(&pool, id, "2026-05-10 12:00:00", "production", 18000.0).await;
        // Negative grid is drawn from the grid, positive is fed back.
        sample(&pool, id, "2026-05-10 07:00:00", "grid", -3600.0).await;
        sample(&pool, id, "2026-05-10 08:00:00", "grid", -7200.0).await;
        sample(&pool, id, "2026-05-10 13:00:00", "grid", 10800.0).await;

        roll_day(&pool, id, day("2026-05-10")).await;

        let got = query_daily_energy(&pool, day("2026-05-10"), day("2026-05-10"))
            .await
            .unwrap();
        assert_eq!(got.len(), 1);
        let d = &got[0];
        assert!((d.consumption_kwh - 0.003).abs() < 1e-9, "{:?}", d);
        assert!((d.production_kwh - 0.005).abs() < 1e-9, "{:?}", d);
        // Import and export must stay separate, not collapse to a net figure.
        assert!((d.grid_import_kwh - 0.003).abs() < 1e-9, "{:?}", d);
        assert!((d.grid_export_kwh - 0.003).abs() < 1e-9, "{:?}", d);
    }

    #[tokio::test]
    async fn rollup_is_idempotent() {
        let pool = init("sqlite::memory:").await.unwrap();
        let id = device(&pool, "10.0.0.2").await;
        sample(&pool, id, "2026-05-10 09:00:00", "consumption", 3600.0).await;

        for _ in 0..3 {
            roll_day(&pool, id, day("2026-05-10")).await;
        }

        let got = query_daily_energy(&pool, day("2026-05-10"), day("2026-05-10"))
            .await
            .unwrap();
        // Re-rolling replaces rather than accumulating.
        assert!((got[0].consumption_kwh - 0.001).abs() < 1e-9, "{got:?}");
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM EnergyDaily")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, DAILY_METRICS.len() as i64);
    }

    #[tokio::test]
    async fn rollup_keeps_days_separate() {
        let pool = init("sqlite::memory:").await.unwrap();
        let id = device(&pool, "10.0.0.3").await;
        // Late on one day and early the next: the local-day boundary must put
        // these in different buckets.
        sample(&pool, id, "2026-05-10 23:59:00", "consumption", 3600.0).await;
        sample(&pool, id, "2026-05-11 00:01:00", "consumption", 7200.0).await;

        roll_day(&pool, id, day("2026-05-10")).await;
        roll_day(&pool, id, day("2026-05-11")).await;

        let got = query_daily_energy(&pool, day("2026-05-10"), day("2026-05-11"))
            .await
            .unwrap();
        assert_eq!(got.len(), 2);
        assert!((got[0].consumption_kwh - 0.001).abs() < 1e-9, "{got:?}");
        assert!((got[1].consumption_kwh - 0.002).abs() < 1e-9, "{got:?}");
    }

    #[tokio::test]
    async fn rollup_writes_nothing_for_a_day_with_no_samples() {
        let pool = init("sqlite::memory:").await.unwrap();
        let id = device(&pool, "10.0.0.4").await;
        sample(&pool, id, "2026-05-10 09:00:00", "consumption", 3600.0).await;

        roll_day(&pool, id, day("2026-05-09")).await;

        // A day the device was offline stays absent, rather than being recorded
        // as a day of zero usage.
        let got = query_daily_energy(&pool, day("2026-05-09"), day("2026-05-09"))
            .await
            .unwrap();
        assert!(got.is_empty(), "{got:?}");
    }

    #[tokio::test]
    async fn query_daily_energy_sums_over_devices_and_skips_gaps() {
        let pool = init("sqlite::memory:").await.unwrap();
        let a = device(&pool, "10.0.0.5").await;
        let b = device(&pool, "10.0.0.6").await;
        sample(&pool, a, "2026-05-10 09:00:00", "consumption", 3600.0).await;
        sample(&pool, b, "2026-05-10 09:00:00", "consumption", 7200.0).await;
        // Nothing at all on the 11th; the 12th has data again.
        sample(&pool, a, "2026-05-12 09:00:00", "consumption", 3600.0).await;

        for d in ["2026-05-10", "2026-05-11", "2026-05-12"] {
            roll_day(&pool, a, day(d)).await;
            roll_day(&pool, b, day(d)).await;
        }

        let got = query_daily_energy(&pool, day("2026-05-10"), day("2026-05-12"))
            .await
            .unwrap();
        // Two days present, the empty one omitted rather than zero-filled.
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].day, day("2026-05-10"));
        assert!((got[0].consumption_kwh - 0.003).abs() < 1e-9, "{got:?}");
        assert_eq!(got[1].day, day("2026-05-12"));
    }

    /// Today, as the local-date string the daily tier keys on.
    fn local_today_string() -> String {
        chrono::Local::now()
            .date_naive()
            .format("%Y-%m-%d")
            .to_string()
    }

    /// Rolls the minute and daily tiers over today for one device.
    async fn roll_up_today(pool: &SqlitePool, device_id: i64) {
        let today = chrono::Local::now().date_naive();
        let cutoff = crate::devices::ts(chrono::Utc::now() + chrono::Duration::days(1));
        rollup_energy_minute(pool, device_id, today, &cutoff)
            .await
            .unwrap();
        rollup_energy_day(pool, device_id, today).await.unwrap();
    }

    /// Reads one metric out of `EnergyDaily` for a day.
    async fn daily(pool: &SqlitePool, day: &str, metric: &str) -> Option<f64> {
        sqlx::query_scalar("SELECT energy_wh FROM EnergyDaily WHERE day = ? AND metric = ?")
            .bind(day)
            .bind(metric)
            .fetch_optional(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_day_recorded_before_the_battery_aware_series_keeps_its_old_answer() {
        // Every day already in the record was written without `grid_to_house`,
        // and the minute tier it is re-rolled from has none either. Treating that
        // absence as zero would report that none of the imported energy ever
        // reached the house — every historical day would jump to 100%
        // self-sufficient. It has to fall back to what the old definition
        // assumed: that all of it did.
        let pool = init("sqlite::memory:").await.unwrap();
        let id = device(&pool, "10.0.0.9").await;
        let day = local_today_string();

        sample(
            &pool,
            id,
            &format!("{day} 09:00:00"),
            "consumption",
            3600.0 * 10.0,
        )
        .await;
        sample(
            &pool,
            id,
            &format!("{day} 09:00:00"),
            "production",
            3600.0 * 4.0,
        )
        .await;
        // Signed grid: negative is imported.
        sample(&pool, id, &format!("{day} 09:00:00"), "grid", -3600.0 * 6.0).await;

        roll_up_today(&pool, id).await;

        assert_eq!(daily(&pool, &day, "grid_import").await, Some(6.0));
        assert_eq!(
            daily(&pool, &day, "grid_to_house").await,
            Some(6.0),
            "absent means unknown, and the old answer is the best one available"
        );
    }

    #[tokio::test]
    async fn a_day_with_the_battery_aware_series_uses_it_instead() {
        // The winter night: 12 kWh imported, 2 into the house and 10 into the
        // battery, so only 2 counts against self-sufficiency.
        let pool = init("sqlite::memory:").await.unwrap();
        let id = device(&pool, "10.0.0.9").await;
        let day = local_today_string();
        let at = format!("{day} 02:00:00");

        sample(&pool, id, &at, "consumption", 3600.0 * 2.0).await;
        sample(&pool, id, &at, "production", 0.0).await;
        sample(&pool, id, &at, "grid", -3600.0 * 12.0).await;
        sample(&pool, id, &at, "grid_to_house", 3600.0 * 2.0).await;
        sample(&pool, id, &at, "grid_to_battery", 3600.0 * 10.0).await;

        roll_up_today(&pool, id).await;

        assert_eq!(daily(&pool, &day, "grid_import").await, Some(12.0));
        assert_eq!(daily(&pool, &day, "grid_to_house").await, Some(2.0));
        assert_eq!(daily(&pool, &day, "grid_to_battery").await, Some(10.0));
    }

    #[tokio::test]
    async fn rollup_energy_daily_backfills_then_settles() {
        let pool = init("sqlite::memory:").await.unwrap();
        let id = device(&pool, "10.0.0.7").await;
        let seeded = seed_days(&pool, id, 4, "consumption").await;
        let newest = seeded[0];

        let first = rollup_history(&pool, std::time::Duration::ZERO)
            .await
            .unwrap();
        assert!(
            first >= 4,
            "backfill should cover every day with data: {first}"
        );

        // Second pass has nothing new, so it only re-rolls the most recent days.
        let second = rollup_history(&pool, std::time::Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(second, DAYS_ALWAYS_REROLLED);

        let got = query_daily_energy(&pool, newest - chrono::Duration::days(3), newest)
            .await
            .unwrap();
        assert_eq!(got.len(), 4);
        for d in &got {
            assert!((d.consumption_kwh - 0.001).abs() < 1e-9, "{got:?}");
        }
    }

    #[tokio::test]
    async fn config_returns_none_for_a_setting_never_written() {
        let pool = init("sqlite::memory:").await.unwrap();
        // A fresh DB has no settings; absence must be readable, not an error,
        // because callers fall back to a default on None.
        assert_eq!(get_config(&pool, "theme").await.unwrap(), None);
    }

    #[tokio::test]
    async fn config_round_trips_and_replaces_on_rewrite() {
        let pool = init("sqlite::memory:").await.unwrap();

        set_config(&pool, "theme", "light").await.unwrap();
        assert_eq!(
            get_config(&pool, "theme").await.unwrap().as_deref(),
            Some("light")
        );

        // Writing the same key again replaces rather than failing the PK or
        // leaving two rows — this is what toggling the theme twice does.
        set_config(&pool, "theme", "dark").await.unwrap();
        assert_eq!(
            get_config(&pool, "theme").await.unwrap().as_deref(),
            Some("dark")
        );
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM Config WHERE key = 'theme'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[tokio::test]
    async fn config_keys_are_independent() {
        let pool = init("sqlite::memory:").await.unwrap();
        set_config(&pool, "theme", "light").await.unwrap();
        set_config(&pool, "other", "x").await.unwrap();
        assert_eq!(
            get_config(&pool, "theme").await.unwrap().as_deref(),
            Some("light")
        );
        assert_eq!(
            get_config(&pool, "other").await.unwrap().as_deref(),
            Some("x")
        );
    }

    #[tokio::test]
    async fn network_status_event_round_trip_is_newest_first() {
        let pool = init("sqlite::memory:").await.unwrap();
        let ip: std::net::IpAddr = "192.168.1.1".parse().unwrap();

        record_network_status_event(&pool, ip, Some("Router"), "OK", "LOST")
            .await
            .unwrap();
        record_network_status_event(&pool, ip, Some("Router"), "LOST", "OK")
            .await
            .unwrap();

        let events = query_recent_network_status_events(&pool, 10).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].previous, crate::app::NetworkDeviceStatus::Lost);
        assert_eq!(events[0].current, crate::app::NetworkDeviceStatus::Ok);
        assert_eq!(events[0].label.as_deref(), Some("Router"));
        assert_eq!(events[1].previous, crate::app::NetworkDeviceStatus::Ok);
        assert_eq!(events[1].current, crate::app::NetworkDeviceStatus::Lost);
    }

    #[tokio::test]
    async fn internet_traffic_query_buckets_and_converts_to_kbps() {
        let pool = init("sqlite::memory:").await.unwrap();
        sqlx::query(
            "INSERT INTO Devices (type, name, ip) VALUES ('mikrotik', 'modem', '10.0.0.1')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let device_id: i64 = sqlx::query_scalar("SELECT id FROM Devices WHERE ip = '10.0.0.1'")
            .fetch_one(&pool)
            .await
            .unwrap();

        // Two samples for the same instant, and so the same bucket: 1000 rx bytes
        // and 200 tx bytes spread over the bucket's width.
        sqlx::query(
            "INSERT INTO RawDeviceMeasurements (device_id, timestamp, metric, value)
             VALUES (?, datetime('now'), 'traffic_rx_bytes', 1000)",
        )
        .bind(device_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO RawDeviceMeasurements (device_id, timestamp, metric, value)
             VALUES (?, datetime('now'), 'traffic_tx_bytes', 200)",
        )
        .bind(device_id)
        .execute(&pool)
        .await
        .unwrap();

        let data = query_internet_traffic_today(&pool).await.unwrap();
        assert_eq!(data.rx_kbps.len(), 1);
        assert_eq!(data.tx_kbps.len(), 1);
        // Bytes over the bucket's own width, in kbps — expressed against the
        // constant rather than a literal, so widening the bucket to follow the
        // poll interval cannot leave the conversion behind.
        let bucket_secs = TRAFFIC_BUCKET_MINUTES as f64 * 60.0;
        assert!((data.rx_kbps[0].1 - (1000.0 * 8.0 / 1000.0 / bucket_secs)).abs() < 1e-9);
        assert!((data.tx_kbps[0].1 - (200.0 * 8.0 / 1000.0 / bucket_secs)).abs() < 1e-9);
    }

    #[tokio::test]
    async fn traffic_buckets_are_as_wide_as_the_gap_between_polls() {
        // Two samples a bucket apart must land in different bars rather than
        // being summed into one and reported as twice the rate.
        let pool = init("sqlite::memory:").await.unwrap();
        sqlx::query(
            "INSERT INTO Devices (type, name, ip) VALUES ('mikrotik', 'modem', '10.0.0.1')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let device_id: i64 = sqlx::query_scalar("SELECT id FROM Devices WHERE ip = '10.0.0.1'")
            .fetch_one(&pool)
            .await
            .unwrap();

        // Anchored to local midnight so both land in today and in known buckets.
        for minute in [0, TRAFFIC_BUCKET_MINUTES] {
            sqlx::query(
                "INSERT INTO RawDeviceMeasurements (device_id, timestamp, metric, value)
                 VALUES (?, datetime(date('now','localtime') || ' 06:00:00', '+' || ? || ' minutes', 'utc'),
                         'traffic_rx_bytes', 600)",
            )
            .bind(device_id)
            .bind(minute)
            .execute(&pool)
            .await
            .unwrap();
        }

        let data = query_internet_traffic_today(&pool).await.unwrap();
        assert_eq!(data.rx_kbps.len(), 2, "one bar each: {:?}", data.rx_kbps);
        let gap = data.rx_kbps[1].0 - data.rx_kbps[0].0;
        assert!(
            (gap - TRAFFIC_BUCKET_MINUTES as f64 / 60.0).abs() < 1e-9,
            "bars are {gap} h apart"
        );
    }

    #[tokio::test]
    async fn upsert_device_without_fingerprint_behaves_like_plain_upsert_by_ip() {
        let pool = init("sqlite::memory:").await.unwrap();
        let ip: IpAddr = "10.1.0.1".parse().unwrap();

        upsert_device(&pool, "keba", "Wallbox v1", ip, 5, None)
            .await
            .unwrap();
        upsert_device(&pool, "keba", "Wallbox v2", ip, 5, None)
            .await
            .unwrap();

        let rows: Vec<(i64, String)> =
            sqlx::query_as("SELECT id, name FROM Devices WHERE type = 'keba'")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, "Wallbox v2");
    }

    #[tokio::test]
    async fn upsert_device_migrates_ip_on_fingerprint_match() {
        let pool = init("sqlite::memory:").await.unwrap();
        let old_ip: IpAddr = "10.1.0.2".parse().unwrap();
        let new_ip: IpAddr = "10.1.0.3".parse().unwrap();

        upsert_device(
            &pool,
            "keba",
            "Wallbox",
            old_ip,
            5,
            Some("AA:BB:CC:DD:EE:FF"),
        )
        .await
        .unwrap();
        let id: i64 = sqlx::query_scalar("SELECT id FROM Devices WHERE type = 'keba'")
            .fetch_one(&pool)
            .await
            .unwrap();
        set_device_login(&pool, old_ip, "admin", "hunter2")
            .await
            .unwrap();
        update_device_label(&pool, old_ip, "Garage Wallbox")
            .await
            .unwrap();

        // Same fingerprint, new address: must move the *existing* row, not add one.
        upsert_device(
            &pool,
            "keba",
            "Wallbox",
            new_ip,
            5,
            Some("AA:BB:CC:DD:EE:FF"),
        )
        .await
        .unwrap();

        let rows: Vec<(i64, String, Option<String>, Option<String>)> =
            sqlx::query_as("SELECT id, ip, username, label FROM Devices WHERE type = 'keba'")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(rows.len(), 1, "must not create a second row");
        assert_eq!(rows[0].0, id, "must keep the original id");
        assert_eq!(rows[0].1, new_ip.to_string());
        assert_eq!(rows[0].2.as_deref(), Some("admin"), "credentials preserved");
        assert_eq!(
            rows[0].3.as_deref(),
            Some("Garage Wallbox"),
            "label preserved"
        );
    }

    #[tokio::test]
    async fn upsert_device_merges_history_when_new_ip_already_has_a_row() {
        let pool = init("sqlite::memory:").await.unwrap();
        let old_ip: IpAddr = "10.1.0.4".parse().unwrap();
        let new_ip: IpAddr = "10.1.0.5".parse().unwrap();

        // The real device, known by fingerprint, still at its old address.
        upsert_device(
            &pool,
            "keba",
            "Wallbox",
            old_ip,
            5,
            Some("11:22:33:44:55:66"),
        )
        .await
        .unwrap();
        let winner_id: i64 = sqlx::query_scalar("SELECT id FROM Devices WHERE ip = ?")
            .bind(old_ip.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO RawDeviceMeasurements (device_id, timestamp, metric, value)
             VALUES (?, datetime('now'), 'power', 1.0)",
        )
        .bind(winner_id)
        .execute(&pool)
        .await
        .unwrap();

        // A second, un-fingerprinted row already discovered at the new address
        // (e.g. found by an earlier scan before this feature existed), with its
        // own accumulated history.
        upsert_device(&pool, "keba", "Wallbox", new_ip, 5, None)
            .await
            .unwrap();
        let loser_id: i64 = sqlx::query_scalar("SELECT id FROM Devices WHERE ip = ?")
            .bind(new_ip.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO RawDeviceMeasurements (device_id, timestamp, metric, value)
             VALUES (?, datetime('now'), 'power', 2.0)",
        )
        .bind(loser_id)
        .execute(&pool)
        .await
        .unwrap();

        // Now the fingerprint match arrives for the new address: merge.
        upsert_device(
            &pool,
            "keba",
            "Wallbox",
            new_ip,
            5,
            Some("11:22:33:44:55:66"),
        )
        .await
        .unwrap();

        let rows: Vec<(i64, String)> =
            sqlx::query_as("SELECT id, ip FROM Devices WHERE type = 'keba'")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(rows.len(), 1, "loser row must be removed");
        assert_eq!(rows[0].0, winner_id, "winner keeps its id");
        assert_eq!(rows[0].1, new_ip.to_string());

        let measurement_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM RawDeviceMeasurements")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            measurement_count, 2,
            "both devices' history survives, reassigned to the winner"
        );
        let orphaned: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM RawDeviceMeasurements WHERE device_id != ?")
                .bind(winner_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(orphaned, 0);
    }

    #[tokio::test]
    async fn device_moved_detects_ip_change_and_deletion() {
        let pool = init("sqlite::memory:").await.unwrap();
        let ip: IpAddr = "10.1.0.6".parse().unwrap();
        upsert_device(&pool, "keba", "Wallbox", ip, 5, None)
            .await
            .unwrap();
        let id: i64 = sqlx::query_scalar("SELECT id FROM Devices WHERE ip = ?")
            .bind(ip.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();

        assert!(!device_moved(&pool, id, ip).await.unwrap());

        sqlx::query("UPDATE Devices SET ip = '10.1.0.7' WHERE id = ?")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(device_moved(&pool, id, ip).await.unwrap());

        sqlx::query("DELETE FROM Devices WHERE id = ?")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(device_moved(&pool, id, ip).await.unwrap());
    }

    // ── Cluster ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_cluster_node_id_is_generated_once_and_then_stable() {
        let pool = init("sqlite::memory:").await.unwrap();
        let first = get_or_create_cluster_node_id(&pool).await.unwrap();
        assert_eq!(first.len(), 16, "16 hex characters from 8 random bytes");
        let second = get_or_create_cluster_node_id(&pool).await.unwrap();
        assert_eq!(first, second, "a second call must not generate a new id");

        // Surviving a fresh read of `Config` directly, not just the in-process cache.
        let stored = get_config(&pool, CLUSTER_NODE_ID_KEY).await.unwrap();
        assert_eq!(stored, Some(first));
    }

    #[tokio::test]
    async fn opening_a_new_epoch_closes_whatever_was_open() {
        let pool = init("sqlite::memory:").await.unwrap();
        let t0 = chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let t1 = t0 + chrono::Duration::minutes(30);

        open_leadership_epoch(&pool, "node-a", t0).await.unwrap();
        let open_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM LeadershipEpochs WHERE ended_at IS NULL")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(open_count, 1);

        open_leadership_epoch(&pool, "node-b", t1).await.unwrap();
        let open_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM LeadershipEpochs WHERE ended_at IS NULL")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(open_count, 1, "the previous epoch must have been closed");

        let still_open: String =
            sqlx::query_scalar("SELECT node_id FROM LeadershipEpochs WHERE ended_at IS NULL")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(still_open, "node-b");
    }

    #[tokio::test]
    async fn leadership_at_a_given_instant_reads_the_covering_epoch() {
        let pool = init("sqlite::memory:").await.unwrap();
        let t0 = chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let t1 = t0 + chrono::Duration::hours(1);
        let t2 = t0 + chrono::Duration::hours(2);

        // Before any epoch exists, nothing is known.
        assert_eq!(query_leadership_epoch_at(&pool, t0).await.unwrap(), None);

        open_leadership_epoch(&pool, "node-a", t0).await.unwrap();
        assert_eq!(
            query_leadership_epoch_at(&pool, t0 + chrono::Duration::minutes(30))
                .await
                .unwrap(),
            Some("node-a".to_string()),
            "still inside node-a's open epoch"
        );

        open_leadership_epoch(&pool, "node-b", t1).await.unwrap();
        assert_eq!(
            query_leadership_epoch_at(&pool, t0 + chrono::Duration::minutes(30))
                .await
                .unwrap(),
            Some("node-a".to_string()),
            "a past instant must still resolve to whoever held it then, not the current holder"
        );
        assert_eq!(
            query_leadership_epoch_at(&pool, t2).await.unwrap(),
            Some("node-b".to_string()),
            "node-b's epoch is still open, so a later instant is covered too"
        );
    }

    // The `DOM_CLUSTER_PEER_ADDR`/`DOM_CLUSTER_IS_PRIMARY` environment-variable override is not
    // exercised here: mutating process environment in a test that runs alongside others on the
    // default multi-threaded test runner would be a source of real flakiness, not a meaningful
    // check — the override itself is a one-line `std::env::var` read.

    #[tokio::test]
    async fn cluster_peer_addr_is_none_until_configured_then_reads_it_back() {
        let pool = init("sqlite::memory:").await.unwrap();
        assert_eq!(cluster_peer_addr(&pool).await.unwrap(), None);
        set_config(&pool, CLUSTER_PEER_ADDR_KEY, "dom-b:7878")
            .await
            .unwrap();
        assert_eq!(
            cluster_peer_addr(&pool).await.unwrap(),
            Some("dom-b:7878".to_string())
        );
    }

    #[tokio::test]
    async fn cluster_is_primary_defaults_to_false() {
        let pool = init("sqlite::memory:").await.unwrap();
        assert!(!cluster_is_primary(&pool).await.unwrap());
        set_config(&pool, CLUSTER_IS_PRIMARY_KEY, "true")
            .await
            .unwrap();
        assert!(cluster_is_primary(&pool).await.unwrap());
        set_config(&pool, CLUSTER_IS_PRIMARY_KEY, "false")
            .await
            .unwrap();
        assert!(!cluster_is_primary(&pool).await.unwrap());
    }

    #[tokio::test]
    async fn a_cluster_keypair_is_generated_once_and_then_stable() {
        let pool = init("sqlite::memory:").await.unwrap();
        let first = get_or_create_cluster_keypair(&pool).await.unwrap();
        let second = get_or_create_cluster_keypair(&pool).await.unwrap();
        assert_eq!(
            crate::cluster::public_key_hex(&first),
            crate::cluster::public_key_hex(&second),
            "a second call must reload the same key, not generate a new one"
        );
    }

    #[tokio::test]
    async fn set_cluster_peer_addr_writes_what_cluster_peer_addr_reads_back() {
        let pool = init("sqlite::memory:").await.unwrap();
        assert_eq!(cluster_peer_addr(&pool).await.unwrap(), None);
        set_cluster_peer_addr(&pool, "192.168.1.42:7878")
            .await
            .unwrap();
        assert_eq!(
            cluster_peer_addr(&pool).await.unwrap(),
            Some("192.168.1.42:7878".to_string())
        );
    }

    #[tokio::test]
    async fn cluster_peer_pubkey_is_none_until_pinned_then_reads_it_back() {
        let pool = init("sqlite::memory:").await.unwrap();
        assert_eq!(cluster_peer_pubkey(&pool).await.unwrap(), None);
        set_cluster_peer_pubkey(&pool, "abcd1234").await.unwrap();
        assert_eq!(
            cluster_peer_pubkey(&pool).await.unwrap(),
            Some("abcd1234".to_string())
        );
    }
}
