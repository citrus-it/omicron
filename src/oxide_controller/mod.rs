/*!
 * Library interface to the Oxide Controller mechanisms
 */

mod config;
mod context;
mod controller_client;
mod http_entrypoints_external;
mod http_entrypoints_internal;
mod oxide_controller;

pub use config::ConfigController;
pub use context::ControllerServerContext;
pub use controller_client::ControllerClient;
pub use oxide_controller::OxideController;
pub use oxide_controller::OxideControllerTestInterfaces;

use http_entrypoints_external::controller_external_api;
use http_entrypoints_internal::controller_internal_api;

use slog::Logger;
use std::sync::Arc;
use tokio::task::JoinHandle;
use uuid::Uuid;

/**
 * Run the OpenAPI generator for the external API, which emits the OpenAPI spec
 * to stdout.
 */
pub fn controller_run_openapi_external() {
    controller_external_api().print_openapi();
}

pub struct OxideControllerServer {
    pub apictx: Arc<ControllerServerContext>,
    pub http_server_external: dropshot::HttpServer,
    pub http_server_internal: dropshot::HttpServer,

    join_handle_external: JoinHandle<Result<(), hyper::error::Error>>,
    join_handle_internal: JoinHandle<Result<(), hyper::error::Error>>,
}

impl OxideControllerServer {
    pub async fn start(
        config: &ConfigController,
        rack_id: &Uuid,
        log: &Logger,
    ) -> Result<OxideControllerServer, String> {
        info!(log, "setting up controller server");

        let ctxlog = log.new(o!("component" => "ControllerServerContext"));
        let apictx = ControllerServerContext::new(rack_id, ctxlog);

        let c1 = Arc::clone(&apictx);
        let mut http_server_external = dropshot::HttpServer::new(
            &config.dropshot_external,
            controller_external_api(),
            c1,
            &log.new(o!("component" => "dropshot_external")),
        )
        .map_err(|error| format!("initializing external server: {}", error))?;

        let c2 = Arc::clone(&apictx);
        let mut http_server_internal = dropshot::HttpServer::new(
            &config.dropshot_internal,
            controller_internal_api(),
            c2,
            &log.new(o!("component" => "dropshot_internal")),
        )
        .map_err(|error| format!("initializing internal server: {}", error))?;

        let join_handle_external = http_server_external.run();
        let join_handle_internal = http_server_internal.run();

        Ok(OxideControllerServer {
            apictx,
            http_server_external,
            http_server_internal,
            join_handle_external,
            join_handle_internal,
        })
    }

    pub async fn wait_for_finish(mut self) -> Result<(), String> {
        let result_external = self
            .http_server_external
            .wait_for_shutdown(self.join_handle_external)
            .await;
        let result_internal = self
            .http_server_internal
            .wait_for_shutdown(self.join_handle_internal)
            .await;

        match (result_external, result_internal) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error_external), Err(error_internal)) => {
                return Err(format!(
                    "errors from both external and internal HTTP \
                     servers(external: \"{}\", internal: \"{}\"",
                    error_external, error_internal
                ));
            }
            (Err(error_external), Ok(())) => {
                return Err(format!("external server: {}", error_external));
            }
            (Ok(()), Err(error_internal)) => {
                return Err(format!("internal server: {}", error_internal));
            }
        }
    }
}

/**
 * Run an instance of the API server.
 */
pub async fn controller_run_server(
    config: &ConfigController,
) -> Result<(), String> {
    let log = config
        .log
        .to_logger("oxide-controller")
        .map_err(|message| format!("initializing logger: {}", message))?;
    let rack_id = Uuid::new_v4();
    let server = OxideControllerServer::start(config, &rack_id, &log).await?;
    server.wait_for_finish().await
}
