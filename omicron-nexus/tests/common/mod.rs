/*!
 * Shared integration testing facilities
 */

use dropshot::test_util::ClientTestContext;
use dropshot::test_util::LogContext;
use dropshot::ConfigDropshot;
use dropshot::ConfigLogging;
use dropshot::ConfigLoggingLevel;
use omicron_common::api::external::IdentityMetadata;
use omicron_common::api::internal::nexus::ProducerEndpoint;
use omicron_common::dev;
use oximeter::Metric;
use slog::o;
use slog::Logger;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;
use uuid::Uuid;

pub mod resource_helpers;

const SLED_AGENT_UUID: &str = "b6d65341-167c-41df-9b5c-41cded99c229";
const RACK_UUID: &str = "c19a698f-c6f9-4a17-ae30-20d711b8f7dc";
pub const OXIMETER_UUID: &str = "39e6175b-4df2-4730-b11d-cbc1e60a2e78";
pub const PRODUCER_UUID: &str = "a6458b7d-87c3-4483-be96-854d814c20de";

pub struct ControlPlaneTestContext {
    pub external_client: ClientTestContext,
    pub internal_client: ClientTestContext,
    pub server: omicron_nexus::Server,
    pub database: dev::db::CockroachInstance,
    pub clickhouse: dev::clickhouse::ClickHouseInstance,
    pub logctx: LogContext,
    sled_agent: omicron_sled_agent::sim::Server,
    oximeter: oximeter::Oximeter,
    producer: oximeter::ProducerServer,
}

impl ControlPlaneTestContext {
    pub async fn teardown(mut self) {
        self.server.http_server_external.close().await.unwrap();
        self.server.http_server_internal.close().await.unwrap();
        self.database.cleanup().await.unwrap();
        self.clickhouse.cleanup().await.unwrap();
        self.sled_agent.http_server.close().await.unwrap();
        self.oximeter.close().await.unwrap();
        self.producer.close().await.unwrap();
        self.logctx.cleanup_successful();
    }
}

pub async fn test_setup(test_name: &str) -> ControlPlaneTestContext {
    /*
     * We load as much configuration as we can from the test suite configuration
     * file.  In practice, TestContext requires that:
     *
     * - the Nexus TCP listen port be 0,
     * - the CockroachDB TCP listen port be 0, and
     * - if the log will go to a file then the path must be the sentinel value
     *   "UNUSED".
     * - each Nexus created for testing gets its own id so they don't see each
     *   others sagas and try to recover them
     *
     * (See LogContext::new() for details.)  Given these restrictions, it may
     * seem barely worth reading a config file at all.  However, users can
     * change the logging level and local IP if they want, and as we add more
     * configuration options, we expect many of those can be usefully configured
     * (and reconfigured) for the test suite.
     */
    let config_file_path = Path::new("tests/config.test.toml");
    let mut config = omicron_nexus::Config::from_file(config_file_path)
        .expect("failed to load config.test.toml");
    config.id = Uuid::new_v4();
    let logctx = LogContext::new(test_name, &config.log);
    let rack_id = Uuid::parse_str(RACK_UUID).unwrap();
    let log = &logctx.log;

    /* Start up CockroachDB. */
    let database = dev::test_setup_database(log).await;

    /* Start ClickHouse database server. */
    let clickhouse = dev::clickhouse::ClickHouseInstance::new(0).await.unwrap();

    config.database.url = database.pg_config().clone();
    let server = omicron_nexus::Server::start(&config, &rack_id, &logctx.log)
        .await
        .unwrap();
    let testctx_external = ClientTestContext::new(
        server.http_server_external.local_addr(),
        logctx.log.new(o!("component" => "external client test context")),
    );
    let testctx_internal = ClientTestContext::new(
        server.http_server_internal.local_addr(),
        logctx.log.new(o!("component" => "internal client test context")),
    );

    /* Set up a single sled agent. */
    let sa_id = Uuid::parse_str(SLED_AGENT_UUID).unwrap();
    let sa = start_sled_agent(
        logctx.log.new(o!(
            "component" => "omicron_sled_agent::sim::Server",
            "sled_id" => sa_id.to_string(),
        )),
        server.http_server_internal.local_addr(),
        sa_id,
    )
    .await
    .unwrap();

    // Set up an Oximeter collector server
    let collector_id = Uuid::parse_str(OXIMETER_UUID).unwrap();
    let oximeter = start_oximeter(
        server.http_server_internal.local_addr(),
        clickhouse.port(),
        collector_id,
    )
    .await
    .unwrap();

    // Set up a test metric producer server
    let producer_id = Uuid::parse_str(PRODUCER_UUID).unwrap();
    let producer = start_producer_server(
        server.http_server_internal.local_addr(),
        producer_id,
    )
    .await
    .unwrap();

    ControlPlaneTestContext {
        server,
        external_client: testctx_external,
        internal_client: testctx_internal,
        database,
        clickhouse,
        sled_agent: sa,
        oximeter,
        producer,
        logctx,
    }
}

pub async fn start_sled_agent(
    log: Logger,
    nexus_address: SocketAddr,
    id: Uuid,
) -> Result<omicron_sled_agent::sim::Server, String> {
    let config = omicron_sled_agent::sim::Config {
        id,
        sim_mode: omicron_sled_agent::sim::SimMode::Explicit,
        nexus_address,
        dropshot: ConfigDropshot {
            bind_address: SocketAddr::new("127.0.0.1".parse().unwrap(), 0),
            ..Default::default()
        },
        /* TODO-cleanup this is unused */
        log: ConfigLogging::StderrTerminal { level: ConfigLoggingLevel::Debug },
    };

    omicron_sled_agent::sim::Server::start(&config, &log).await
}

pub async fn start_oximeter(
    nexus_address: SocketAddr,
    db_port: u16,
    id: Uuid,
) -> Result<oximeter::Oximeter, String> {
    let db = oximeter::oximeter_server::DbConfig {
        address: SocketAddr::new("::1".parse().unwrap(), db_port),
        batch_size: 10,
        batch_interval: 10,
    };
    let config = oximeter::oximeter_server::Config {
        id,
        nexus_address,
        db,
        dropshot: ConfigDropshot {
            bind_address: SocketAddr::new("::1".parse().unwrap(), 0),
            ..Default::default()
        },
        log: ConfigLogging::StderrTerminal { level: ConfigLoggingLevel::Error },
    };
    oximeter::Oximeter::new(&config).await.map_err(|e| e.to_string())
}

#[derive(oximeter::Target)]
struct IntegrationTarget {
    pub name: String,
}

#[derive(oximeter::Metric)]
struct IntegrationMetric {
    pub name: String,
    pub datum: i64,
}

// A producer of simple counter metrics used in the integration tests
struct IntegrationProducer {
    pub target: IntegrationTarget,
    pub metric: IntegrationMetric,
}

impl oximeter::Producer for IntegrationProducer {
    fn produce(
        &mut self,
    ) -> Result<
        Box<(dyn Iterator<Item = oximeter::types::Sample> + 'static)>,
        oximeter::Error,
    > {
        let sample = oximeter::types::Sample::new(&self.target, &self.metric);
        *self.metric.datum_mut() += 1;
        Ok(Box::new(vec![sample].into_iter()))
    }
}

pub async fn start_producer_server(
    nexus_address: SocketAddr,
    id: Uuid,
) -> Result<oximeter::ProducerServer, String> {
    // Set up a producer server.
    //
    // This listens on any available port, and the ProducerServer internally updates this to the
    // actual bound port of the Dropshot HTTP server.
    let producer_address = SocketAddr::new("::1".parse().unwrap(), 0);
    let server_info = ProducerEndpoint {
        id,
        address: producer_address,
        base_route: "/collect".to_string(),
        interval: Duration::from_secs(10),
    };
    let registration_info = oximeter::producer_server::RegistrationInfo::new(
        nexus_address,
        "/metrics/producers",
    );
    let config = oximeter::producer_server::ProducerServerConfig {
        server_info,
        registration_info,
        dropshot_config: ConfigDropshot {
            bind_address: producer_address,
            ..Default::default()
        },
        logging_config: ConfigLogging::StderrTerminal {
            level: ConfigLoggingLevel::Error,
        },
    };
    let server = oximeter::ProducerServer::start(&config)
        .await
        .map_err(|e| e.to_string())?;

    // Create and register an actual metric producer.
    let producer = IntegrationProducer {
        target: IntegrationTarget {
            name: "integration-test-target".to_string(),
        },
        metric: IntegrationMetric {
            name: "integration-test-metric".to_string(),
            datum: 0,
        },
    };
    server
        .collector()
        .register_producer(Box::new(producer))
        .map_err(|e| e.to_string())?;
    Ok(server)
}

/** Returns whether the two identity metadata objects are identical. */
pub fn identity_eq(ident1: &IdentityMetadata, ident2: &IdentityMetadata) {
    assert_eq!(ident1.id, ident2.id);
    assert_eq!(ident1.name, ident2.name);
    assert_eq!(ident1.description, ident2.description);
    assert_eq!(ident1.time_created, ident2.time_created);
    assert_eq!(ident1.time_modified, ident2.time_modified);
}
