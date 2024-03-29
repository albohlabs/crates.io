use super::{MockAnonymousUser, MockCookieUser, MockTokenUser};
use crate::util::chaosproxy::ChaosProxy;
use crate::util::github::{MockGitHubClient, MOCK_GITHUB_DATA};
use anyhow::Context;
use crates_io::config::{
    self, BalanceCapacityConfig, Base, CdnLogQueueConfig, CdnLogStorageConfig, DatabasePools,
    DbPoolConfig,
};
use crates_io::middleware::cargo_compat::StatusCodeConfig;
use crates_io::models::token::{CrateScope, EndpointScope};
use crates_io::rate_limiter::{LimitedAction, RateLimiterConfig};
use crates_io::storage::StorageConfig;
use crates_io::team_repo::MockTeamRepo;
use crates_io::worker::{Environment, RunnerExt};
use crates_io::{App, Emails, Env};
use crates_io_index::testing::UpstreamIndex;
use crates_io_index::{Credentials, RepositoryConfig};
use crates_io_test_db::TestDatabase;
use crates_io_worker::Runner;
use diesel::PgConnection;
use futures_util::TryStreamExt;
use oauth2::{ClientId, ClientSecret};
use std::collections::HashSet;
use std::{rc::Rc, sync::Arc, time::Duration};
use tokio::runtime::Runtime;

struct TestAppInner {
    pub runtime: Runtime,

    app: Arc<App>,
    router: axum::Router,
    index: Option<UpstreamIndex>,
    runner: Option<Runner<Arc<Environment>>>,

    primary_db_chaosproxy: Option<Arc<ChaosProxy>>,
    replica_db_chaosproxy: Option<Arc<ChaosProxy>>,

    // Must be the last field of the struct!
    test_database: Option<TestDatabase>,
}

impl Drop for TestAppInner {
    fn drop(&mut self) {
        use crates_io::schema::background_jobs;
        use diesel::prelude::*;

        // Avoid a double-panic if the test is already failing
        if std::thread::panicking() {
            return;
        }

        // Lazily run any remaining jobs
        if let Some(runner) = &self.runner {
            let handle = runner.start();
            self.runtime.block_on(handle.wait_for_shutdown());

            runner.check_for_failed_jobs().expect("Failed jobs remain");
        }

        // Manually verify that all jobs have completed successfully
        // This will catch any tests that enqueued a job but forgot to initialize the runner
        let conn = &mut *self.app.db_write().unwrap();
        let job_count: i64 = background_jobs::table.count().get_result(conn).unwrap();
        assert_eq!(
            0, job_count,
            "Unprocessed or failed jobs remain in the queue"
        );

        // TODO: If a runner was started, obtain the clone from it and ensure its HEAD matches the upstream index HEAD

        // We manually close the connection pools here to prevent their `Drop`
        // implementation from failing because no tokio runtime is running.
        {
            let _rt_guard = self.runtime.enter();
            self.app.deadpool_primary.close();
            if let Some(pool) = &self.app.deadpool_replica {
                pool.close();
            }
        }
    }
}

/// A representation of the app and its database transaction
#[derive(Clone)]
pub struct TestApp(Rc<TestAppInner>);

impl TestApp {
    /// Initialize an application with an `Uploader` that panics
    pub fn init() -> TestAppBuilder {
        crates_io::util::tracing::init_for_test();

        TestAppBuilder {
            config: simple_config(),
            index: None,
            build_job_runner: false,
            use_chaos_proxy: false,
            team_repo: MockTeamRepo::new(),
        }
    }

    /// Initialize a full application, with a proxy, index, and background worker
    pub fn full() -> TestAppBuilder {
        Self::init().with_git_index().with_job_runner()
    }

    /// Obtain the database connection and pass it to the closure
    ///
    /// Within each test, the connection pool only has 1 connection so it is necessary to drop the
    /// connection before making any API calls.  Once the closure returns, the connection is
    /// dropped, ensuring it is returned to the pool and available for any future API calls.
    pub fn db<T, F: FnOnce(&mut PgConnection) -> T>(&self, f: F) -> T {
        match self.0.test_database.as_ref() {
            Some(test_database) => f(&mut test_database.connect()),
            None => f(&mut self.0.app.db_write().unwrap()),
        }
    }

    /// Create a new user with a verified email address in the database and return a mock user
    /// session
    ///
    /// This method updates the database directly
    pub fn db_new_user(&self, username: &str) -> MockCookieUser {
        use crates_io::schema::emails;
        use diesel::prelude::*;

        let user = self.db(|conn| {
            let email = "something@example.com";

            let user = crate::new_user(username)
                .create_or_update(None, &self.0.app.emails, conn)
                .unwrap();
            diesel::insert_into(emails::table)
                .values((
                    emails::user_id.eq(user.id),
                    emails::email.eq(email),
                    emails::verified.eq(true),
                ))
                .execute(conn)
                .unwrap();
            user
        });
        MockCookieUser {
            app: self.clone(),
            user,
        }
    }

    pub fn runtime(&self) -> &Runtime {
        &self.0.runtime
    }

    /// Obtain a reference to the upstream repository ("the index")
    pub fn upstream_index(&self) -> &UpstreamIndex {
        assert_some!(self.0.index.as_ref())
    }

    /// Obtain a list of crates from the index HEAD
    pub fn crates_from_index_head(&self, crate_name: &str) -> Vec<crates_io_index::Crate> {
        self.upstream_index()
            .crates_from_index_head(crate_name)
            .unwrap()
    }

    pub fn stored_files(&self) -> Vec<String> {
        let store = self.as_inner().storage.as_inner();

        let rt = self.runtime();

        let list = rt.block_on(async {
            let stream = store.list(None);
            stream.try_collect::<Vec<_>>().await.unwrap()
        });

        list.into_iter()
            .map(|meta| meta.location.to_string())
            .collect()
    }

    #[track_caller]
    pub fn run_pending_background_jobs(&self) {
        let runner = &self.0.runner;
        let runner = runner.as_ref().expect("Index has not been initialized");

        let handle = runner.start();
        self.runtime().block_on(handle.wait_for_shutdown());

        runner
            .check_for_failed_jobs()
            .expect("Could not determine if jobs failed");
    }

    /// Obtain a reference to the inner `App` value
    pub fn as_inner(&self) -> &App {
        &self.0.app
    }

    /// Obtain a reference to the axum Router
    pub fn router(&self) -> &axum::Router {
        &self.0.router
    }

    pub(crate) fn primary_db_chaosproxy(&self) -> Arc<ChaosProxy> {
        self.0
            .primary_db_chaosproxy
            .clone()
            .expect("ChaosProxy is not enabled on this test, call with_database during app init")
    }

    pub(crate) fn replica_db_chaosproxy(&self) -> Arc<ChaosProxy> {
        self.0
            .replica_db_chaosproxy
            .clone()
            .expect("ChaosProxy is not enabled on this test, call with_database during app init")
    }
}

pub struct TestAppBuilder {
    config: config::Server,
    index: Option<UpstreamIndex>,
    build_job_runner: bool,
    use_chaos_proxy: bool,
    team_repo: MockTeamRepo,
}

impl TestAppBuilder {
    /// Create a `TestApp` with an empty database
    pub fn empty(mut self) -> (TestApp, MockAnonymousUser) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("Failed to initialize tokio runtime")
            .unwrap();

        // Run each test inside a fresh database schema, deleted at the end of the test,
        // The schema will be cleared up once the app is dropped.
        let (primary_db_chaosproxy, replica_db_chaosproxy, test_database) = {
            let test_database = TestDatabase::new();

            let primary_proxy = if self.use_chaos_proxy {
                let (primary_proxy, url) =
                    ChaosProxy::proxy_database_url(test_database.url()).unwrap();

                self.config.db.primary.url = url.into();
                Some(primary_proxy)
            } else {
                self.config.db.primary.url = test_database.url().to_string().into();
                None
            };

            let replica_proxy = self.config.db.replica.as_mut().and_then(|replica| {
                if self.use_chaos_proxy {
                    let (primary_proxy, url) =
                        ChaosProxy::proxy_database_url(test_database.url()).unwrap();

                    replica.url = url.into();
                    Some(primary_proxy)
                } else {
                    replica.url = test_database.url().to_string().into();
                    None
                }
            });

            (primary_proxy, replica_proxy, Some(test_database))
        };

        let (app, router) = build_app(self.config);

        let runner = if self.build_job_runner {
            let index = self
                .index
                .as_ref()
                .expect("Index must be initialized to build a job runner");

            let repository_config = RepositoryConfig {
                index_location: index.url(),
                credentials: Credentials::Missing,
            };

            let environment = Environment::builder()
                .config(app.config.clone())
                .repository_config(repository_config)
                .storage(app.storage.clone())
                .connection_pool(app.primary_database.clone())
                .deadpool(app.deadpool_primary.clone())
                .emails(app.emails.clone())
                .team_repo(Box::new(self.team_repo))
                .build()
                .unwrap();

            let runner = Runner::new(
                runtime.handle(),
                (*app.primary_database).clone(),
                Arc::new(environment),
            )
            .shutdown_when_queue_empty()
            .register_crates_io_job_types();

            Some(runner)
        } else {
            None
        };

        let test_app_inner = TestAppInner {
            runtime,
            app,
            test_database,
            router,
            index: self.index,
            runner,
            primary_db_chaosproxy,
            replica_db_chaosproxy,
        };
        let test_app = TestApp(Rc::new(test_app_inner));
        let anon = MockAnonymousUser {
            app: test_app.clone(),
        };
        (test_app, anon)
    }

    // Create a `TestApp` with a database including a default user
    pub fn with_user(self) -> (TestApp, MockAnonymousUser, MockCookieUser) {
        let (app, anon) = self.empty();
        let user = app.db_new_user("foo");
        (app, anon, user)
    }

    /// Create a `TestApp` with a database including a default user and its token
    pub fn with_token(self) -> (TestApp, MockAnonymousUser, MockCookieUser, MockTokenUser) {
        let (app, anon) = self.empty();
        let user = app.db_new_user("foo");
        let token = user.db_new_token("bar");
        (app, anon, user, token)
    }

    pub fn with_scoped_token(
        self,
        crate_scopes: Option<Vec<CrateScope>>,
        endpoint_scopes: Option<Vec<EndpointScope>>,
    ) -> (TestApp, MockAnonymousUser, MockCookieUser, MockTokenUser) {
        let (app, anon) = self.empty();
        let user = app.db_new_user("foo");
        let token = user.db_new_scoped_token("bar", crate_scopes, endpoint_scopes, None);
        (app, anon, user, token)
    }

    pub fn with_config(mut self, f: impl FnOnce(&mut config::Server)) -> Self {
        f(&mut self.config);
        self
    }

    pub fn with_rate_limit(self, action: LimitedAction, rate: Duration, burst: i32) -> Self {
        self.with_config(|config| {
            config
                .rate_limiter
                .insert(action, RateLimiterConfig { rate, burst });
        })
    }

    pub fn with_git_index(mut self) -> Self {
        self.index = Some(UpstreamIndex::new().unwrap());
        self
    }

    pub fn with_job_runner(mut self) -> Self {
        self.build_job_runner = true;
        self
    }

    pub fn with_chaos_proxy(mut self) -> Self {
        self.use_chaos_proxy = true;
        self
    }

    pub fn with_team_repo(mut self, team_repo: MockTeamRepo) -> Self {
        self.team_repo = team_repo;
        self
    }

    pub fn with_replica(mut self) -> Self {
        let primary = &self.config.db.primary;

        self.config.db.replica = Some(DbPoolConfig {
            url: primary.url.clone(),
            read_only_mode: true,
            pool_size: primary.pool_size,
            async_pool_size: primary.async_pool_size,
            min_idle: primary.min_idle,
        });

        self
    }
}

fn simple_config() -> config::Server {
    let base = Base { env: Env::Test };

    let db = DatabasePools {
        primary: DbPoolConfig {
            // This value is supposed be overridden by the
            // `TestAppBuilder::empty()` fn. If it's not, then
            // something is broken.
            url: String::from("invalid default url").into(),
            read_only_mode: false,
            pool_size: 3,
            async_pool_size: 2,
            min_idle: None,
        },
        replica: None,
        tcp_timeout_ms: 1000, // 1 second
        connection_timeout: Duration::from_secs(1),
        statement_timeout: Duration::from_secs(1),
        helper_threads: 1,
        enforce_tls: false,
    };

    let balance_capacity = BalanceCapacityConfig {
        report_only: false,
        log_total_at_count: 50,
        log_at_percentage: 50,
        throttle_at_percentage: 70,
        dl_only_at_percentage: 80,
    };

    let mut storage = StorageConfig::in_memory();
    storage.cdn_prefix = Some("static.crates.io".to_string());

    config::Server {
        base,
        ip: [127, 0, 0, 1].into(),
        port: 8888,
        max_blocking_threads: None,
        db,
        storage,
        cdn_log_queue: CdnLogQueueConfig::Mock,
        cdn_log_storage: CdnLogStorageConfig::memory(),
        session_key: cookie::Key::derive_from("test this has to be over 32 bytes long".as_bytes()),
        gh_client_id: ClientId::new(dotenvy::var("GH_CLIENT_ID").unwrap_or_default()),
        gh_client_secret: ClientSecret::new(dotenvy::var("GH_CLIENT_SECRET").unwrap_or_default()),
        max_upload_size: 128 * 1024, // 128 kB should be enough for most testing purposes
        max_unpack_size: 128 * 1024, // 128 kB should be enough for most testing purposes
        max_features: 10,
        max_dependencies: 10,
        rate_limiter: Default::default(),
        new_version_rate_limit: Some(10),
        blocked_traffic: Default::default(),
        blocked_ips: Default::default(),
        max_allowed_page_offset: 200,
        page_offset_ua_blocklist: vec![],
        page_offset_cidr_blocklist: vec![],
        excluded_crate_names: vec![],
        domain_name: "crates.io".into(),
        allowed_origins: Default::default(),
        downloads_persist_interval: Duration::from_secs(1),
        ownership_invitations_expiration_days: 30,
        metrics_authorization_token: None,
        instance_metrics_log_every_seconds: None,
        blocked_routes: HashSet::new(),
        version_id_cache_size: 10000,
        version_id_cache_ttl: Duration::from_secs(5 * 60),
        cdn_user_agent: "Amazon CloudFront".to_string(),
        balance_capacity,

        // The middleware has its own unit tests to verify its functionality.
        // Here, we can test what would happen if we toggled the status code
        // enforcement off eventually.
        cargo_compat_status_code_config: StatusCodeConfig::Disabled,

        // The frontend code is not needed for the backend tests.
        serve_dist: false,
        serve_html: false,
        content_security_policy: None,
    }
}

fn build_app(config: config::Server) -> (Arc<App>, axum::Router) {
    // Use the in-memory email backend for all tests, allowing tests to analyze the emails sent by
    // the application. This will also prevent cluttering the filesystem.
    let emails = Emails::new_in_memory();

    // Use a custom mock for the GitHub client, allowing to define the GitHub users and
    // organizations without actually having to create GitHub accounts.
    let github = Box::new(MockGitHubClient::new(&MOCK_GITHUB_DATA));

    let app = App::new(config, emails, github);

    let app = Arc::new(app);
    let router = crates_io::build_handler(Arc::clone(&app));
    (app, router)
}
