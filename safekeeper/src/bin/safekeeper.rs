//
// Main entry point for the safekeeper executable
//
use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use camino::{Utf8Path, Utf8PathBuf};
use clap::{ArgAction, Parser};
use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use futures::{FutureExt, StreamExt};
use http_utils::tls_certs::ReloadingCertificateResolver;
use metrics::set_build_info_metric;
use remote_storage::RemoteStorageConfig;
use safekeeper::defaults::{
    DEFAULT_CONTROL_FILE_SAVE_INTERVAL, DEFAULT_EVICTION_MIN_RESIDENT,
    DEFAULT_GLOBAL_DISK_CHECK_INTERVAL, DEFAULT_HEARTBEAT_TIMEOUT, DEFAULT_HTTP_LISTEN_ADDR,
    DEFAULT_MAX_GLOBAL_DISK_USAGE_RATIO, DEFAULT_MAX_OFFLOADER_LAG_BYTES,
    DEFAULT_MAX_REELECT_OFFLOADER_LAG_BYTES, DEFAULT_MAX_TIMELINE_DISK_USAGE_BYTES,
    DEFAULT_PARTIAL_BACKUP_CONCURRENCY, DEFAULT_PARTIAL_BACKUP_TIMEOUT, DEFAULT_PG_LISTEN_ADDR,
    DEFAULT_SSL_CERT_FILE, DEFAULT_SSL_CERT_RELOAD_PERIOD, DEFAULT_SSL_KEY_FILE,
};
use safekeeper::hadron;
use safekeeper::wal_backup::WalBackup;
use safekeeper::{
    BACKGROUND_RUNTIME, BROKER_RUNTIME, GlobalTimelines, HTTP_RUNTIME, SafeKeeperConf,
    WAL_SERVICE_RUNTIME, broker, control_file, http, wal_service,
};
use sd_notify::NotifyState;
use storage_broker::{DEFAULT_ENDPOINT, Uri};
use tokio::runtime::Handle;
use tokio::signal::unix::{SignalKind, signal};
use tokio::task::JoinError;
use tracing::*;
use utils::auth::{JwtAuth, Scope, SwappableJwtAuth};
use utils::id::NodeId;
use utils::logging::{self, LogFormat, SecretString};
use utils::metrics_collector::{METRICS_COLLECTION_INTERVAL, METRICS_COLLECTOR};
use utils::sentry_init::init_sentry;
use utils::{pid_file, project_build_tag, project_git_version, tcp_listener};

use safekeeper::hadron::{
    GLOBAL_DISK_LIMIT_EXCEEDED, get_filesystem_capacity, get_filesystem_usage,
};
use safekeeper::metrics::GLOBAL_DISK_UTIL_CHECK_SECONDS;
use std::sync::atomic::Ordering;

#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// Configure jemalloc to profile heap allocations by sampling stack traces every 2 MB (1 << 21).
/// This adds roughly 3% overhead for allocations on average, which is acceptable considering
/// performance-sensitive code will avoid allocations as far as possible anyway.
#[allow(non_upper_case_globals)]
#[unsafe(export_name = "malloc_conf")]
pub static malloc_conf: &[u8] = b"prof:true,prof_active:true,lg_prof_sample:21\0";

const PID_FILE_NAME: &str = "safekeeper.pid";
const ID_FILE_NAME: &str = "safekeeper.id";

project_git_version!(GIT_VERSION);
project_build_tag!(BUILD_TAG);

const FEATURES: &[&str] = &[
    #[cfg(feature = "testing")]
    "testing",
];

fn version() -> String {
    format!(
        "{GIT_VERSION} failpoints: {}, features: {:?}",
        fail::has_failpoints(),
        FEATURES,
    )
}

const ABOUT: &str = r#"
A fleet of safekeepers is responsible for reliably storing WAL received from
compute, passing it through consensus (mitigating potential computes brain
split), and serving the hardened part further downstream to pageserver(s).
"#;

#[derive(Parser)]
#[command(name = "Neon safekeeper", version = GIT_VERSION, about = ABOUT, long_about = None)]
struct Args {
    /// Path to the safekeeper data directory.
    #[arg(short = 'D', long, default_value = "./")]
    datadir: Utf8PathBuf,
    /// Safekeeper node id.
    #[arg(long)]
    id: Option<u64>,
    /// Initialize safekeeper with given id and exit.
    #[arg(long)]
    init: bool,
    /// Listen endpoint for receiving/sending WAL in the form host:port.
    #[arg(short, long, default_value = DEFAULT_PG_LISTEN_ADDR)]
    listen_pg: String,
    /// Listen endpoint for receiving/sending WAL in the form host:port allowing
    /// only tenant scoped auth tokens. Pointless if auth is disabled.
    #[arg(long, default_value = None, verbatim_doc_comment)]
    listen_pg_tenant_only: Option<String>,
    /// Listen http endpoint for management and metrics in the form host:port.
    #[arg(long, default_value = DEFAULT_HTTP_LISTEN_ADDR)]
    listen_http: String,
    /// Listen https endpoint for management and metrics in the form host:port.
    #[arg(long, default_value = None)]
    listen_https: Option<String>,
    /// Advertised endpoint for receiving/sending WAL in the form host:port. If not
    /// specified, listen_pg is used to advertise instead.
    #[arg(long, default_value = None)]
    advertise_pg: Option<String>,
    /// Availability zone of the safekeeper.
    #[arg(long)]
    availability_zone: Option<String>,
    /// Do not wait for changes to be written safely to disk. Unsafe.
    #[arg(short, long)]
    no_sync: bool,
    /// Dump control file at path specified by this argument and exit.
    #[arg(long)]
    dump_control_file: Option<Utf8PathBuf>,
    /// Broker endpoint for storage nodes coordination in the form
    /// http[s]://host:port. In case of https schema TLS is connection is
    /// established; plaintext otherwise.
    #[arg(long, default_value = DEFAULT_ENDPOINT, verbatim_doc_comment)]
    broker_endpoint: Uri,
    /// Broker keepalive interval.
    #[arg(long, value_parser= humantime::parse_duration, default_value = storage_broker::DEFAULT_KEEPALIVE_INTERVAL)]
    broker_keepalive_interval: Duration,
    /// Peer safekeeper is considered dead after not receiving heartbeats from
    /// it during this period passed as a human readable duration.
    #[arg(long, value_parser= humantime::parse_duration, default_value = DEFAULT_HEARTBEAT_TIMEOUT, verbatim_doc_comment)]
    heartbeat_timeout: Duration,
    /// Enable/disable peer recovery.
    #[arg(long, default_value = "false", action=ArgAction::Set)]
    peer_recovery: bool,
    /// Remote storage configuration for WAL backup (offloading to s3) as TOML
    /// inline table, e.g.
    ///   {max_concurrent_syncs = 17, max_sync_errors = 13, bucket_name = "<BUCKETNAME>", bucket_region = "<REGION>", concurrency_limit = 119}
    /// Safekeeper offloads WAL to
    ///   [prefix_in_bucket/]<tenant_id>/<timeline_id>/<segment_file>, mirroring
    /// structure on the file system.
    #[arg(long, value_parser = parse_remote_storage, verbatim_doc_comment)]
    remote_storage: Option<RemoteStorageConfig>,
    /// Safekeeper won't be elected for WAL offloading if it is lagging for more than this value in bytes
    #[arg(long, default_value_t = DEFAULT_MAX_OFFLOADER_LAG_BYTES)]
    max_offloader_lag: u64,
    /* BEGIN_HADRON */
    /// Safekeeper will re-elect a new offloader if the current backup lagging for more than this value in bytes
    #[arg(long, default_value_t = DEFAULT_MAX_REELECT_OFFLOADER_LAG_BYTES)]
    max_reelect_offloader_lag_bytes: u64,
    /// Safekeeper will stop accepting new WALs if the timeline disk usage exceeds this value in bytes.
    /// Setting this value to 0 disables the limit.
    #[arg(long, default_value_t = DEFAULT_MAX_TIMELINE_DISK_USAGE_BYTES)]
    max_timeline_disk_usage_bytes: u64,
    /* END_HADRON */
    /// Number of max parallel WAL segments to be offloaded to remote storage.
    #[arg(long, default_value = "5")]
    wal_backup_parallel_jobs: usize,
    /// Disable WAL backup to s3. When disabled, safekeeper removes WAL ignoring
    /// WAL backup horizon.
    #[arg(long)]
    disable_wal_backup: bool,
    /// If given, enables auth on incoming connections to WAL service endpoint
    /// (--listen-pg). Value specifies path to a .pem public key used for
    /// validations of JWT tokens. Empty string is allowed and means disabling
    /// auth.
    #[arg(long, verbatim_doc_comment, value_parser = opt_pathbuf_parser)]
    pg_auth_public_key_path: Option<Utf8PathBuf>,
    /// If given, enables auth on incoming connections to tenant only WAL
    /// service endpoint (--listen-pg-tenant-only). Value specifies path to a
    /// .pem public key used for validations of JWT tokens. Empty string is
    /// allowed and means disabling auth.
    #[arg(long, verbatim_doc_comment, value_parser = opt_pathbuf_parser)]
    pg_tenant_only_auth_public_key_path: Option<Utf8PathBuf>,
    /// If given, enables auth on incoming connections to http management
    /// service endpoint (--listen-http). Value specifies path to a .pem public
    /// key used for validations of JWT tokens. Empty string is allowed and
    /// means disabling auth.
    #[arg(long, verbatim_doc_comment, value_parser = opt_pathbuf_parser)]
    http_auth_public_key_path: Option<Utf8PathBuf>,
    /// Format for logging, either 'plain' or 'json'.
    #[arg(long, default_value = "plain")]
    log_format: String,
    /// Run everything in single threaded current thread runtime, might be
    /// useful for debugging.
    #[arg(long)]
    current_thread_runtime: bool,
    /// Keep horizon for walsenders, i.e. don't remove WAL segments that are
    /// still needed for existing replication connection.
    #[arg(long)]
    walsenders_keep_horizon: bool,
    /// Controls how long backup will wait until uploading the partial segment.
    #[arg(long, value_parser = humantime::parse_duration, default_value = DEFAULT_PARTIAL_BACKUP_TIMEOUT, verbatim_doc_comment)]
    partial_backup_timeout: Duration,
    /// Disable task to push messages to broker every second. Supposed to
    /// be used in tests.
    #[arg(long)]
    disable_periodic_broker_push: bool,
    /// Enable automatic switching to offloaded state.
    #[arg(long)]
    enable_offload: bool,
    /// Delete local WAL files after offloading. When disabled, they will be left on disk.
    #[arg(long)]
    delete_offloaded_wal: bool,
    /// Pending updates to control file will be automatically saved after this interval.
    #[arg(long, value_parser = humantime::parse_duration, default_value = DEFAULT_CONTROL_FILE_SAVE_INTERVAL)]
    control_file_save_interval: Duration,
    /// Number of allowed concurrent uploads of partial segments to remote storage.
    #[arg(long, default_value = DEFAULT_PARTIAL_BACKUP_CONCURRENCY)]
    partial_backup_concurrency: usize,
    /// How long a timeline must be resident before it is eligible for eviction.
    /// Usually, timeline eviction has to wait for `partial_backup_timeout` before being eligible for eviction,
    /// but if a timeline is un-evicted and then _not_ written to, it would immediately flap to evicting again,
    /// if it weren't for `eviction_min_resident` preventing that.
    ///
    /// Also defines interval for eviction retries.
    #[arg(long, value_parser = humantime::parse_duration, default_value = DEFAULT_EVICTION_MIN_RESIDENT)]
    eviction_min_resident: Duration,
    /// Enable fanning out WAL to different shards from the same reader
    #[arg(long)]
    wal_reader_fanout: bool,
    /// Only fan out the WAL reader if the absoulte delta between the new requested position
    /// and the current position of the reader is smaller than this value.
    #[arg(long)]
    max_delta_for_fanout: Option<u64>,
    /// Path to a file with certificate's private key for https API.
    #[arg(long, default_value = DEFAULT_SSL_KEY_FILE)]
    ssl_key_file: Utf8PathBuf,
    /// Path to a file with a X509 certificate for https API.
    #[arg(long, default_value = DEFAULT_SSL_CERT_FILE)]
    ssl_cert_file: Utf8PathBuf,
    /// Period to reload certificate and private key from files.
    #[arg(long, value_parser = humantime::parse_duration, default_value = DEFAULT_SSL_CERT_RELOAD_PERIOD)]
    ssl_cert_reload_period: Duration,
    /// Trusted root CA certificates to use in https APIs.
    #[arg(long)]
    ssl_ca_file: Option<Utf8PathBuf>,
    /// Flag to use https for requests to peer's safekeeper API.
    #[arg(long)]
    use_https_safekeeper_api: bool,
    /// Path to the JWT auth token used to authenticate with other safekeepers.
    #[arg(long)]
    auth_token_path: Option<Utf8PathBuf>,

    /// Enable TLS in WAL service API.
    /// Does not force TLS: the client negotiates TLS usage during the handshake.
    /// Uses key and certificate from ssl_key_file/ssl_cert_file.
    #[arg(long)]
    enable_tls_wal_service_api: bool,

    /// Controls whether to collect all metrics on each scrape or to return potentially stale
    /// results.
    #[arg(long, default_value_t = true)]
    force_metric_collection_on_scrape: bool,

    /// Run in development mode (disables security checks)
    #[arg(long, help = "Run in development mode (disables security checks)")]
    dev: bool,
    /* BEGIN_HADRON */
    #[arg(long)]
    enable_pull_timeline_on_startup: bool,
    /// How often to scan entire data-dir for total disk usage
    #[arg(long, value_parser=humantime::parse_duration, default_value = DEFAULT_GLOBAL_DISK_CHECK_INTERVAL)]
    global_disk_check_interval: Duration,
    /// The portion of the filesystem capacity that can be used by all timelines.
    /// A circuit breaker will trip and reject all WAL writes if the total usage
    /// exceeds this ratio.
    /// Set to 0 to disable the global disk usage limit.
    #[arg(long, default_value_t = DEFAULT_MAX_GLOBAL_DISK_USAGE_RATIO)]
    max_global_disk_usage_ratio: f64,
    /* END_HADRON */
}

// Like PathBufValueParser, but allows empty string.
fn opt_pathbuf_parser(s: &str) -> Result<Utf8PathBuf, String> {
    Ok(Utf8PathBuf::from_str(s).unwrap())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // We want to allow multiple occurences of the same arg (taking the last) so
    // that neon_local could generate command with defaults + overrides without
    // getting 'argument cannot be used multiple times' error. This seems to be
    // impossible with pure Derive API, so convert struct to Command, modify it,
    // parse arguments, and then fill the struct back.
    let cmd = <Args as clap::CommandFactory>::command()
        .args_override_self(true)
        .version(version());
    let mut matches = cmd.get_matches();
    let mut args = <Args as clap::FromArgMatches>::from_arg_matches_mut(&mut matches)?;

    // I failed to modify opt_pathbuf_parser to return Option<PathBuf> in
    // reasonable time, so turn empty string into option post factum.
    if let Some(pb) = &args.pg_auth_public_key_path {
        if pb.as_os_str().is_empty() {
            args.pg_auth_public_key_path = None;
        }
    }
    if let Some(pb) = &args.pg_tenant_only_auth_public_key_path {
        if pb.as_os_str().is_empty() {
            args.pg_tenant_only_auth_public_key_path = None;
        }
    }
    if let Some(pb) = &args.http_auth_public_key_path {
        if pb.as_os_str().is_empty() {
            args.http_auth_public_key_path = None;
        }
    }

    if let Some(addr) = args.dump_control_file {
        let state = control_file::FileStorage::load_control_file(addr)?;
        let json = serde_json::to_string(&state)?;
        print!("{json}");
        return Ok(());
    }

    // important to keep the order of:
    // 1. init logging
    // 2. tracing panic hook
    // 3. sentry
    logging::init(
        LogFormat::from_config(&args.log_format)?,
        logging::TracingErrorLayerEnablement::Disabled,
        logging::Output::Stdout,
    )?;
    logging::replace_panic_hook_with_tracing_panic_hook().forget();
    info!("version: {GIT_VERSION}");
    info!("buld_tag: {BUILD_TAG}");

    let args_workdir = &args.datadir;
    let workdir = args_workdir.canonicalize_utf8().with_context(|| {
        format!("Failed to get the absolute path for input workdir {args_workdir:?}")
    })?;

    // Change into the data directory.
    std::env::set_current_dir(&workdir)?;

    // Prevent running multiple safekeepers on the same directory
    let lock_file_path = workdir.join(PID_FILE_NAME);
    let lock_file =
        pid_file::claim_for_current_process(&lock_file_path).context("claim pid file")?;
    info!("claimed pid file at {lock_file_path:?}");
    // ensure that the lock file is held even if the main thread of the process is panics
    // we need to release the lock file only when the current process is gone
    std::mem::forget(lock_file);

    // Set or read our ID.
    let id = set_id(&workdir, args.id.map(NodeId))?;
    if args.init {
        return Ok(());
    }

    let pg_auth = match args.pg_auth_public_key_path.as_ref() {
        None => {
            info!("pg auth is disabled");
            None
        }
        Some(path) => {
            info!("loading pg auth JWT key from {path}");
            Some(Arc::new(
                JwtAuth::from_key_path(path).context("failed to load the auth key")?,
            ))
        }
    };
    let pg_tenant_only_auth = match args.pg_tenant_only_auth_public_key_path.as_ref() {
        None => {
            info!("pg tenant only auth is disabled");
            None
        }
        Some(path) => {
            info!("loading pg tenant only auth JWT key from {path}");
            Some(Arc::new(
                JwtAuth::from_key_path(path).context("failed to load the auth key")?,
            ))
        }
    };
    let http_auth = match args.http_auth_public_key_path.as_ref() {
        None => {
            info!("http auth is disabled");
            None
        }
        Some(path) => {
            info!("loading http auth JWT key(s) from {path}");
            let jwt_auth = JwtAuth::from_key_path(path).context("failed to load the auth key")?;
            Some(Arc::new(SwappableJwtAuth::new(jwt_auth)))
        }
    };

    // Load JWT auth token to connect to other safekeepers for pull_timeline.
    let sk_auth_token = if let Some(auth_token_path) = args.auth_token_path.as_ref() {
        info!("loading JWT token for authentication with safekeepers from {auth_token_path}");
        let auth_token = tokio::fs::read_to_string(auth_token_path).await?;
        Some(SecretString::from(auth_token.trim().to_owned()))
    } else {
        info!("no JWT token for authentication with safekeepers detected");
        None
    };

    let ssl_ca_certs = match args.ssl_ca_file.as_ref() {
        Some(ssl_ca_file) => {
            tracing::info!("Using ssl root CA file: {ssl_ca_file:?}");
            let buf = tokio::fs::read(ssl_ca_file).await?;
            pem::parse_many(&buf)?
                .into_iter()
                .filter(|pem| pem.tag() == "CERTIFICATE")
                .collect()
        }
        None => Vec::new(),
    };

    let conf = Arc::new(SafeKeeperConf {
        workdir,
        my_id: id,
        listen_pg_addr: args.listen_pg,
        listen_pg_addr_tenant_only: args.listen_pg_tenant_only,
        listen_http_addr: args.listen_http,
        listen_https_addr: args.listen_https,
        advertise_pg_addr: args.advertise_pg,
        availability_zone: args.availability_zone,
        no_sync: args.no_sync,
        broker_endpoint: args.broker_endpoint,
        broker_keepalive_interval: args.broker_keepalive_interval,
        heartbeat_timeout: args.heartbeat_timeout,
        peer_recovery_enabled: args.peer_recovery,
        remote_storage: args.remote_storage,
        max_offloader_lag_bytes: args.max_offloader_lag,
        /* BEGIN_HADRON */
        max_reelect_offloader_lag_bytes: args.max_reelect_offloader_lag_bytes,
        max_timeline_disk_usage_bytes: args.max_timeline_disk_usage_bytes,
        /* END_HADRON */
        wal_backup_enabled: !args.disable_wal_backup,
        backup_parallel_jobs: args.wal_backup_parallel_jobs,
        pg_auth,
        pg_tenant_only_auth,
        http_auth,
        sk_auth_token,
        current_thread_runtime: args.current_thread_runtime,
        walsenders_keep_horizon: args.walsenders_keep_horizon,
        partial_backup_timeout: args.partial_backup_timeout,
        disable_periodic_broker_push: args.disable_periodic_broker_push,
        enable_offload: args.enable_offload,
        delete_offloaded_wal: args.delete_offloaded_wal,
        control_file_save_interval: args.control_file_save_interval,
        partial_backup_concurrency: args.partial_backup_concurrency,
        eviction_min_resident: args.eviction_min_resident,
        wal_reader_fanout: args.wal_reader_fanout,
        max_delta_for_fanout: args.max_delta_for_fanout,
        ssl_key_file: args.ssl_key_file,
        ssl_cert_file: args.ssl_cert_file,
        ssl_cert_reload_period: args.ssl_cert_reload_period,
        ssl_ca_certs,
        use_https_safekeeper_api: args.use_https_safekeeper_api,
        enable_tls_wal_service_api: args.enable_tls_wal_service_api,
        force_metric_collection_on_scrape: args.force_metric_collection_on_scrape,
        /* BEGIN_HADRON */
        advertise_pg_addr_tenant_only: None,
        enable_pull_timeline_on_startup: args.enable_pull_timeline_on_startup,
        hcc_base_url: None,
        global_disk_check_interval: args.global_disk_check_interval,
        max_global_disk_usage_ratio: args.max_global_disk_usage_ratio,
        /* END_HADRON */
    });

    // initialize sentry if SENTRY_DSN is provided
    let _sentry_guard = init_sentry(
        Some(GIT_VERSION.into()),
        &[("node_id", &conf.my_id.to_string())],
    );
    start_safekeeper(conf).await
}

/// Result of joining any of main tasks: upper error means task failed to
/// complete, e.g. panicked, inner is error produced by task itself.
type JoinTaskRes = Result<anyhow::Result<()>, JoinError>;

async fn start_safekeeper(conf: Arc<SafeKeeperConf>) -> Result<()> {
    // fsync the datadir to make sure we have a consistent state on disk.
    if !conf.no_sync {
        let dfd = File::open(&conf.workdir).context("open datadir for syncfs")?;
        let started = Instant::now();
        utils::crashsafe::syncfs(dfd)?;
        let elapsed = started.elapsed();
        info!(
            elapsed_ms = elapsed.as_millis(),
            "syncfs data directory done"
        );
    }

    info!("starting safekeeper WAL service on {}", conf.listen_pg_addr);
    let pg_listener = tcp_listener::bind(conf.listen_pg_addr.clone()).map_err(|e| {
        error!("failed to bind to address {}: {}", conf.listen_pg_addr, e);
        e
    })?;

    let pg_listener_tenant_only =
        if let Some(listen_pg_addr_tenant_only) = &conf.listen_pg_addr_tenant_only {
            info!(
                "starting safekeeper tenant scoped WAL service on {}",
                listen_pg_addr_tenant_only
            );
            let listener = tcp_listener::bind(listen_pg_addr_tenant_only.clone()).map_err(|e| {
                error!(
                    "failed to bind to address {}: {}",
                    listen_pg_addr_tenant_only, e
                );
                e
            })?;
            Some(listener)
        } else {
            None
        };

    info!(
        "starting safekeeper HTTP service on {}",
        conf.listen_http_addr
    );
    let http_listener = tcp_listener::bind(conf.listen_http_addr.clone()).map_err(|e| {
        error!("failed to bind to address {}: {}", conf.listen_http_addr, e);
        e
    })?;

    let https_listener = match conf.listen_https_addr.as_ref() {
        Some(listen_https_addr) => {
            info!("starting safekeeper HTTPS service on {}", listen_https_addr);
            Some(tcp_listener::bind(listen_https_addr).map_err(|e| {
                error!("failed to bind to address {}: {}", listen_https_addr, e);
                e
            })?)
        }
        None => None,
    };

    let wal_backup = Arc::new(WalBackup::new(&conf).await?);

    let global_timelines = Arc::new(GlobalTimelines::new(conf.clone(), wal_backup.clone()));

    // Register metrics collector for active timelines. It's important to do this
    // after daemonizing, otherwise process collector will be upset.
    let timeline_collector = safekeeper::metrics::TimelineCollector::new(global_timelines.clone());
    metrics::register_internal(Box::new(timeline_collector))?;

    // Keep handles to main tasks to die if any of them disappears.
    let mut tasks_handles: FuturesUnordered<BoxFuture<(String, JoinTaskRes)>> =
        FuturesUnordered::new();

    // Start wal backup launcher before loading timelines as we'll notify it
    // through the channel about timelines which need offloading, not draining
    // the channel would cause deadlock.
    let current_thread_rt = conf
        .current_thread_runtime
        .then(|| Handle::try_current().expect("no runtime in main"));

    // Load all timelines from disk to memory.
    global_timelines.init().await?;

    /* BEGIN_HADRON */
    if conf.enable_pull_timeline_on_startup && global_timelines.timelines_count() == 0 {
        match hadron::hcc_pull_timelines(&conf, global_timelines.clone()).await {
            Ok(_) => {
                info!("Successfully pulled all timelines from peer safekeepers");
            }
            Err(e) => {
                error!("Failed to pull timelines from peer safekeepers: {:?}", e);
                return Err(e);
            }
        }
    }
    /* END_HADRON */

    // Run everything in current thread rt, if asked.
    if conf.current_thread_runtime {
        info!("running in current thread runtime");
    }

    let tls_server_config = if conf.listen_https_addr.is_some() || conf.enable_tls_wal_service_api {
        let ssl_key_file = conf.ssl_key_file.clone();
        let ssl_cert_file = conf.ssl_cert_file.clone();
        let ssl_cert_reload_period = conf.ssl_cert_reload_period;

        // Create resolver in BACKGROUND_RUNTIME, so the background certificate reloading
        // task is run in this runtime.
        let cert_resolver = current_thread_rt
            .as_ref()
            .unwrap_or_else(|| BACKGROUND_RUNTIME.handle())
            .spawn(async move {
                ReloadingCertificateResolver::new(
                    "main",
                    &ssl_key_file,
                    &ssl_cert_file,
                    ssl_cert_reload_period,
                )
                .await
            })
            .await??;

        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_cert_resolver(cert_resolver);

        Some(Arc::new(config))
    } else {
        None
    };

    let wal_service_handle = current_thread_rt
        .as_ref()
        .unwrap_or_else(|| WAL_SERVICE_RUNTIME.handle())
        .spawn(wal_service::task_main(
            conf.clone(),
            pg_listener,
            Scope::SafekeeperData,
            conf.enable_tls_wal_service_api
                .then(|| tls_server_config.clone())
                .flatten(),
            global_timelines.clone(),
        ))
        // wrap with task name for error reporting
        .map(|res| ("WAL service main".to_owned(), res));
    tasks_handles.push(Box::pin(wal_service_handle));

    let global_timelines_ = global_timelines.clone();
    let timeline_housekeeping_handle = current_thread_rt
        .as_ref()
        .unwrap_or_else(|| WAL_SERVICE_RUNTIME.handle())
        .spawn(async move {
            const TOMBSTONE_TTL: Duration = Duration::from_secs(3600 * 24);
            loop {
                tokio::time::sleep(TOMBSTONE_TTL).await;
                global_timelines_.housekeeping(&TOMBSTONE_TTL);
            }
        })
        .map(|res| ("Timeline map housekeeping".to_owned(), res));
    tasks_handles.push(Box::pin(timeline_housekeeping_handle));

    /* BEGIN_HADRON */
    // Spawn global disk usage watcher task, if a global disk usage limit is specified.
    let interval = conf.global_disk_check_interval;
    let data_dir = conf.workdir.clone();
    // Use the safekeeper data directory to compute filesystem capacity. This only runs once on startup, so
    // there is little point to continue if we can't have the proper protections in place.
    let fs_capacity_bytes = get_filesystem_capacity(data_dir.as_std_path())
        .expect("Failed to get filesystem capacity for data directory");
    let limit: u64 = (conf.max_global_disk_usage_ratio * fs_capacity_bytes as f64) as u64;
    if limit > 0 {
        let disk_usage_watch_handle = BACKGROUND_RUNTIME
            .handle()
            .spawn(async move {
                // Use Tokio interval to preserve fixed cadence between filesystem utilization checks
                let mut ticker = tokio::time::interval(interval);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

                loop {
                    ticker.tick().await;
                    let data_dir_clone = data_dir.clone();
                    let check_start = Instant::now();

                    let usage = tokio::task::spawn_blocking(move || {
                        get_filesystem_usage(data_dir_clone.as_std_path())
                    })
                    .await
                    .unwrap_or(0);

                    let elapsed = check_start.elapsed().as_secs_f64();
                    GLOBAL_DISK_UTIL_CHECK_SECONDS.observe(elapsed);
                    if usage > limit {
                        warn!(
                            "Global disk usage exceeded limit. Usage: {} bytes, limit: {} bytes",
                            usage, limit
                        );
                    }
                    GLOBAL_DISK_LIMIT_EXCEEDED.store(usage > limit, Ordering::Relaxed);
                }
            })
            .map(|res| ("Global disk usage watcher".to_string(), res));
        tasks_handles.push(Box::pin(disk_usage_watch_handle));
    }
    /* END_HADRON */
    if let Some(pg_listener_tenant_only) = pg_listener_tenant_only {
        let wal_service_handle = current_thread_rt
            .as_ref()
            .unwrap_or_else(|| WAL_SERVICE_RUNTIME.handle())
            .spawn(wal_service::task_main(
                conf.clone(),
                pg_listener_tenant_only,
                Scope::Tenant,
                conf.enable_tls_wal_service_api
                    .then(|| tls_server_config.clone())
                    .flatten(),
                global_timelines.clone(),
            ))
            // wrap with task name for error reporting
            .map(|res| ("WAL service tenant only main".to_owned(), res));
        tasks_handles.push(Box::pin(wal_service_handle));
    }

    let http_handle = current_thread_rt
        .as_ref()
        .unwrap_or_else(|| HTTP_RUNTIME.handle())
        .spawn(http::task_main_http(
            conf.clone(),
            http_listener,
            global_timelines.clone(),
        ))
        .map(|res| ("HTTP service main".to_owned(), res));
    tasks_handles.push(Box::pin(http_handle));

    if let Some(https_listener) = https_listener {
        let https_handle = current_thread_rt
            .as_ref()
            .unwrap_or_else(|| HTTP_RUNTIME.handle())
            .spawn(http::task_main_https(
                conf.clone(),
                https_listener,
                tls_server_config.expect("tls_server_config is set earlier if https is enabled"),
                global_timelines.clone(),
            ))
            .map(|res| ("HTTPS service main".to_owned(), res));
        tasks_handles.push(Box::pin(https_handle));
    }

    let broker_task_handle = current_thread_rt
        .as_ref()
        .unwrap_or_else(|| BROKER_RUNTIME.handle())
        .spawn(
            broker::task_main(conf.clone(), global_timelines.clone())
                .instrument(info_span!("broker")),
        )
        .map(|res| ("broker main".to_owned(), res));
    tasks_handles.push(Box::pin(broker_task_handle));

    /* BEGIN_HADRON */
    if conf.force_metric_collection_on_scrape {
        let metrics_handle = current_thread_rt
            .as_ref()
            .unwrap_or_else(|| BACKGROUND_RUNTIME.handle())
            .spawn(async move {
                let mut interval: tokio::time::Interval =
                    tokio::time::interval(METRICS_COLLECTION_INTERVAL);
                loop {
                    interval.tick().await;
                    tokio::task::spawn_blocking(|| {
                        METRICS_COLLECTOR.run_once(true);
                    });
                }
            })
            .map(|res| ("broker main".to_owned(), res));
        tasks_handles.push(Box::pin(metrics_handle));
    }
    /* END_HADRON */

    set_build_info_metric(GIT_VERSION, BUILD_TAG);

    // TODO: update tokio-stream, convert to real async Stream with
    // SignalStream, map it to obtain missing signal name, combine streams into
    // single stream we can easily sit on.
    let mut sigquit_stream = signal(SignalKind::quit())?;
    let mut sigint_stream = signal(SignalKind::interrupt())?;
    let mut sigterm_stream = signal(SignalKind::terminate())?;

    // Notify systemd that we are ready. This is important as currently loading
    // timelines takes significant time (~30s in busy regions).
    if let Err(e) = sd_notify::notify(true, &[NotifyState::Ready]) {
        warn!("systemd notify failed: {:?}", e);
    }

    tokio::select! {
        Some((task_name, res)) = tasks_handles.next()=> {
            error!("{} task failed: {:?}, exiting", task_name, res);
            std::process::exit(1);
        }
        // On any shutdown signal, log receival and exit. Additionally, handling
        // SIGQUIT prevents coredump.
        _ = sigquit_stream.recv() => info!("received SIGQUIT, terminating"),
        _ = sigint_stream.recv() => info!("received SIGINT, terminating"),
        _ = sigterm_stream.recv() => info!("received SIGTERM, terminating")

    };
    std::process::exit(0);
}

/// Determine safekeeper id.
fn set_id(workdir: &Utf8Path, given_id: Option<NodeId>) -> Result<NodeId> {
    let id_file_path = workdir.join(ID_FILE_NAME);

    let my_id: NodeId;
    // If file with ID exists, read it in; otherwise set one passed.
    match fs::read(&id_file_path) {
        Ok(id_serialized) => {
            my_id = NodeId(
                std::str::from_utf8(&id_serialized)
                    .context("failed to parse safekeeper id")?
                    .parse()
                    .context("failed to parse safekeeper id")?,
            );
            if let Some(given_id) = given_id {
                if given_id != my_id {
                    bail!(
                        "safekeeper already initialized with id {}, can't set {}",
                        my_id,
                        given_id
                    );
                }
            }
            info!("safekeeper ID {}", my_id);
        }
        Err(error) => match error.kind() {
            ErrorKind::NotFound => {
                my_id = if let Some(given_id) = given_id {
                    given_id
                } else {
                    bail!("safekeeper id is not specified");
                };
                let mut f = File::create(&id_file_path)
                    .with_context(|| format!("Failed to create id file at {id_file_path:?}"))?;
                f.write_all(my_id.to_string().as_bytes())?;
                f.sync_all()?;
                info!("initialized safekeeper id {}", my_id);
            }
            _ => {
                return Err(error.into());
            }
        },
    }
    Ok(my_id)
}

fn parse_remote_storage(storage_conf: &str) -> anyhow::Result<RemoteStorageConfig> {
    RemoteStorageConfig::from_toml(&storage_conf.parse()?)
}

#[test]
fn verify_cli() {
    use clap::CommandFactory;
    Args::command().debug_assert()
}
