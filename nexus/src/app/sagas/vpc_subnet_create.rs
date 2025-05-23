// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use super::ACTION_GENERATE_ID;
use super::ActionRegistry;
use super::NexusActionContext;
use super::NexusSaga;
use crate::app::sagas::declare_saga_actions;
use crate::external_api::params;
use nexus_db_lookup::LookupPath;
use nexus_db_queries::db::queries::vpc_subnet::InsertVpcSubnetError;
use nexus_db_queries::{authn, authz, db};
use omicron_common::api::external;
use oxnet::IpNet;
use oxnet::Ipv6Net;
use serde::Deserialize;
use serde::Serialize;
use steno::ActionError;
use steno::Node;
use uuid::Uuid;

// vpc subnet create saga: input parameters

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Params {
    pub serialized_authn: authn::saga::Serialized,
    pub subnet_create: params::VpcSubnetCreate,
    /// We create at most one IPv6 block in the subnet, but have a retry loop
    /// in case of collisions when randomly generating a block. Our random
    /// choices are fixed ahead of saga start for idempotency.
    ///
    /// This field must contain at least one entry, or we'll fail with a 500.
    pub potential_ipv6_blocks: Vec<Ipv6Net>,
    pub authz_vpc: authz::Vpc,
    pub authz_system_router: authz::VpcRouter,
    pub custom_router: Option<authz::VpcRouter>,
}

// vpc subnet create saga: actions

declare_saga_actions! {
    vpc_subnet_create;
    VPC_SUBNET_CREATE_SUBNET -> "subnet" {
        + svsc_create_subnet
        - svsc_create_subnet_undo
    }
    VPC_SUBNET_CREATE_SYS_ROUTE -> "route" {
        + svsc_create_route
        - svsc_create_route_undo
    }
    VPC_SUBNET_CREATE_LINK_CUSTOM -> "output" {
        + svsc_link_custom
        - svsc_link_custom_undo
    }
    VPC_NOTIFY_RPW -> "notified" {
        + svsc_notify_rpw
    }
}

// vpc subnet create saga: definition

#[derive(Debug)]
pub(crate) struct SagaVpcSubnetCreate;
impl NexusSaga for SagaVpcSubnetCreate {
    const NAME: &'static str = "vpc-subnet-create";
    type Params = Params;

    fn register_actions(registry: &mut ActionRegistry) {
        vpc_subnet_create_register_actions(registry);
    }

    fn make_saga_dag(
        _params: &Self::Params,
        mut builder: steno::DagBuilder,
    ) -> Result<steno::Dag, super::SagaInitError> {
        builder.append(Node::action(
            "subnet_id",
            "GenerateVpcSubnetId",
            ACTION_GENERATE_ID.as_ref(),
        ));
        builder.append(Node::action(
            "route_id",
            "GenerateRouteId",
            ACTION_GENERATE_ID.as_ref(),
        ));

        builder.append(vpc_subnet_create_subnet_action());
        builder.append(vpc_subnet_create_sys_route_action());
        builder.append(vpc_subnet_create_link_custom_action());
        builder.append(vpc_notify_rpw_action());

        Ok(builder.build()?)
    }
}

// vpc subnet create saga: action implementations

async fn svsc_create_subnet(
    sagactx: NexusActionContext,
) -> Result<(authz::VpcSubnet, db::model::VpcSubnet), ActionError> {
    let osagactx = sagactx.user_data();
    let params = sagactx.saga_params::<Params>()?;
    let opctx = crate::context::op_context_for_saga_action(
        &sagactx,
        &params.serialized_authn,
    );
    let log = sagactx.user_data().log();

    let subnet_id = sagactx.lookup::<Uuid>("subnet_id")?;
    let vpc_id = params.authz_vpc.id();

    let num_retries = params.potential_ipv6_blocks.len();
    let retryable = num_retries > 1;

    let mut result = None;
    for ipv6_block in params.potential_ipv6_blocks {
        let subnet = db::model::VpcSubnet::new(
            subnet_id,
            vpc_id,
            params.subnet_create.identity.clone(),
            params.subnet_create.ipv4_block,
            ipv6_block,
        );

        result = Some(
            osagactx
                .datastore()
                .vpc_create_subnet(&opctx, &params.authz_vpc, subnet)
                .await,
        );

        if matches!(result, Some(Ok(_))) {
            break;
        }

        match &result {
            Some(Ok(_)) => break,
            // Allow NUM_RETRIES retries, after the first attempt.
            //
            // Note that we only catch IPv6 overlaps. The client
            // always specifies the IPv4 range, so we fail the
            // request if that overlaps with an existing range.
            Some(Err(InsertVpcSubnetError::OverlappingIpRange(IpNet::V6(
                _,
            )))) if retryable => debug!(
                log,
                "autogenerated random IPv6 range overlap";
                "subnet_id" => ?subnet_id,
                "ipv6_block" => %ipv6_block
            ),
            _ => {}
        }
    }

    (match result {
        None => Err(external::Error::internal_error(
            "Attempted to create VPC subnet without any IPv6 allocation",
        )),
        Some(Err(InsertVpcSubnetError::OverlappingIpRange(IpNet::V6(_))))
            if retryable =>
        {
            // TODO-monitoring TODO-debugging
            //
            // We should maintain a counter for this occurrence, and
            // export that via `oximeter`, so that we can see these
            // failures through the timeseries database. The main
            // goal here is for us to notice that this is happening
            // before it becomes a major issue for customers.
            error!(
                log,
                "failed to generate unique random IPv6 address \
                range in {} retries",
                num_retries;
                "vpc_id" => ?vpc_id,
                "subnet_id" => ?subnet_id,
            );
            Err(external::Error::internal_error(
                "Unable to allocate unique IPv6 address range \
                for VPC Subnet",
            ))
        }
        // Overlapping IPv4/explicit v6 range, which is a client error.
        Some(Err(e @ InsertVpcSubnetError::OverlappingIpRange(_))) => {
            Err(e.into_external())
        }
        Some(Err(InsertVpcSubnetError::External(e))) => Err(e),
        Some(Ok(v)) => Ok(v),
    })
    .map_err(ActionError::action_failed)
}

async fn svsc_create_subnet_undo(
    sagactx: NexusActionContext,
) -> Result<(), anyhow::Error> {
    let osagactx = sagactx.user_data();
    let params = sagactx.saga_params::<Params>()?;
    let opctx = crate::context::op_context_for_saga_action(
        &sagactx,
        &params.serialized_authn,
    );

    let (authz_subnet, db_subnet) =
        sagactx.lookup::<(authz::VpcSubnet, db::model::VpcSubnet)>("subnet")?;

    let res = osagactx
        .datastore()
        .vpc_delete_subnet_raw(&opctx, &db_subnet, &authz_subnet)
        .await;

    match res {
        Ok(_) | Err(external::Error::ObjectNotFound { .. }) => {
            let _ = osagactx
                .datastore()
                .vpc_increment_rpw_version(&opctx, params.authz_vpc.id())
                .await;
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

async fn svsc_create_route(
    sagactx: NexusActionContext,
) -> Result<authz::RouterRoute, ActionError> {
    let osagactx = sagactx.user_data();
    let params = sagactx.saga_params::<Params>()?;
    let opctx = crate::context::op_context_for_saga_action(
        &sagactx,
        &params.serialized_authn,
    );

    let route_id = sagactx.lookup::<Uuid>("route_id")?;
    let (.., db_subnet) =
        sagactx.lookup::<(authz::VpcSubnet, db::model::VpcSubnet)>("subnet")?;

    let out = osagactx
        .datastore()
        .vpc_create_subnet_route(
            &opctx,
            &params.authz_system_router,
            &db_subnet,
            route_id,
        )
        .await;

    match out {
        Ok((auth, ..)) => Ok(auth),
        Err(external::Error::ObjectAlreadyExists { .. }) => {
            LookupPath::new(&opctx, osagactx.datastore())
                .router_route_id(route_id)
                .lookup_for(authz::Action::Read)
                .await
                .map_err(ActionError::action_failed)
                .map(|(.., v)| v)
        }
        Err(e) => Err(ActionError::action_failed(e)),
    }
}

async fn svsc_create_route_undo(
    sagactx: NexusActionContext,
) -> Result<(), anyhow::Error> {
    let osagactx = sagactx.user_data();
    let params = sagactx.saga_params::<Params>()?;
    let opctx = crate::context::op_context_for_saga_action(
        &sagactx,
        &params.serialized_authn,
    );

    let authz_route = sagactx.lookup::<authz::RouterRoute>("route")?;

    match osagactx.datastore().router_delete_route(&opctx, &authz_route).await {
        Ok(_) | Err(external::Error::ObjectNotFound { .. }) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

async fn svsc_link_custom(
    sagactx: NexusActionContext,
) -> Result<db::model::VpcSubnet, ActionError> {
    let osagactx = sagactx.user_data();
    let params = sagactx.saga_params::<Params>()?;
    let opctx = crate::context::op_context_for_saga_action(
        &sagactx,
        &params.serialized_authn,
    );
    let (authz_subnet, db_subnet) =
        sagactx.lookup::<(authz::VpcSubnet, db::model::VpcSubnet)>("subnet")?;

    if let Some(custom_router) = params.custom_router {
        osagactx
            .datastore()
            .vpc_subnet_set_custom_router(&opctx, &authz_subnet, &custom_router)
            .await
            .map_err(ActionError::action_failed)
    } else {
        Ok(db_subnet)
    }
}

async fn svsc_link_custom_undo(
    sagactx: NexusActionContext,
) -> Result<(), anyhow::Error> {
    let osagactx = sagactx.user_data();
    let params = sagactx.saga_params::<Params>()?;
    let opctx = crate::context::op_context_for_saga_action(
        &sagactx,
        &params.serialized_authn,
    );
    let (authz_subnet, ..) =
        sagactx.lookup::<(authz::VpcSubnet, db::model::VpcSubnet)>("subnet")?;

    if params.custom_router.is_some() {
        let _ = osagactx
            .datastore()
            .vpc_subnet_unset_custom_router(&opctx, &authz_subnet)
            .await;
    }

    Ok(())
}

async fn svsc_notify_rpw(
    sagactx: NexusActionContext,
) -> Result<(), ActionError> {
    let osagactx = sagactx.user_data();
    let params = sagactx.saga_params::<Params>()?;
    let opctx = crate::context::op_context_for_saga_action(
        &sagactx,
        &params.serialized_authn,
    );

    osagactx
        .datastore()
        .vpc_increment_rpw_version(&opctx, params.authz_vpc.id())
        .await
        .map_err(ActionError::action_failed)
}

#[cfg(test)]
pub(crate) mod test {
    use crate::app::saga::create_saga_dag;
    use crate::app::sagas::test_helpers;
    use crate::{
        app::sagas::vpc_subnet_create::Params,
        app::sagas::vpc_subnet_create::SagaVpcSubnetCreate,
        external_api::params,
    };
    use async_bb8_diesel::AsyncRunQueryDsl;
    use diesel::{ExpressionMethods, QueryDsl, SelectableHelper};
    use dropshot::test_util::ClientTestContext;
    use nexus_db_lookup::LookupPath;
    use nexus_db_model::RouterRouteKind;
    use nexus_db_queries::db;
    use nexus_db_queries::{
        authn::saga::Serialized, authz, context::OpContext,
        db::datastore::DataStore, db::fixed_data::vpc::SERVICES_VPC_ID,
    };
    use nexus_test_utils::resource_helpers::create_default_ip_pool;
    use nexus_test_utils::resource_helpers::create_project;
    use nexus_test_utils_macros::nexus_test;
    use nexus_types::external_api::params::VpcSelector;
    use omicron_common::api::external::NameOrId;
    use omicron_common::api::external::{
        self, IdentityMetadataCreateParams, Ipv6NetExt,
    };
    use uuid::Uuid;

    type ControlPlaneTestContext =
        nexus_test_utils::ControlPlaneTestContext<crate::Server>;

    const PROJECT_NAME: &str = "springfield-squidport";

    async fn create_org_and_project(client: &ClientTestContext) -> Uuid {
        create_default_ip_pool(&client).await;
        let project = create_project(client, PROJECT_NAME).await;
        project.identity.id
    }

    // Helper for creating VPC subnet create parameters
    fn new_test_params(
        opctx: &OpContext,
        authz_vpc: authz::Vpc,
        db_vpc: db::model::Vpc,
        authz_system_router: authz::VpcRouter,
    ) -> Params {
        let ipv6_block = db_vpc
            .ipv6_prefix
            .random_subnet(oxnet::Ipv6Net::VPC_SUBNET_IPV6_PREFIX_LENGTH)
            .map(|block| block.0)
            .unwrap();

        Params {
            serialized_authn: Serialized::for_opctx(opctx),
            subnet_create: params::VpcSubnetCreate {
                identity: IdentityMetadataCreateParams {
                    name: "my-subnet".parse().unwrap(),
                    description: "My New Subnet.".to_string(),
                },
                ipv4_block: "192.168.0.0/24".parse().unwrap(),
                ipv6_block: None,
                custom_router: None,
            },
            potential_ipv6_blocks: vec![ipv6_block],
            authz_vpc,
            authz_system_router,
            custom_router: None,
        }
    }

    fn test_opctx(cptestctx: &ControlPlaneTestContext) -> OpContext {
        OpContext::for_tests(
            cptestctx.logctx.log.new(o!()),
            cptestctx.server.server_context().nexus.datastore().clone(),
        )
    }

    async fn get_vpc_state(
        cptestctx: &ControlPlaneTestContext,
        project_id: Uuid,
    ) -> (authz::Vpc, db::model::Vpc, authz::VpcRouter) {
        let nexus = &cptestctx.server.server_context().nexus;
        let opctx = test_opctx(&cptestctx);
        let datastore = nexus.datastore();
        let (.., authz_vpc, db_vpc) = nexus
            .vpc_lookup(
                &opctx,
                VpcSelector {
                    vpc: NameOrId::Name("default".parse().unwrap()),
                    project: Some(project_id.into()),
                },
            )
            .unwrap()
            .fetch()
            .await
            .unwrap();
        let (.., authz_system_router) = LookupPath::new(&opctx, datastore)
            .vpc_router_id(db_vpc.system_router_id)
            .lookup_for(authz::Action::CreateChild)
            .await
            .unwrap();

        (authz_vpc, db_vpc, authz_system_router)
    }

    pub(crate) async fn verify_clean_slate(
        datastore: &DataStore,
        vpc_id: Uuid,
    ) {
        assert!(one_vpc_route_exists(datastore).await);
        assert!(one_subnet_exists(datastore, vpc_id).await);
    }

    async fn one_vpc_route_exists(datastore: &DataStore) -> bool {
        use nexus_db_queries::db::model::RouterRoute;
        use nexus_db_schema::schema::router_route::dsl;
        use nexus_db_schema::schema::vpc_router::dsl as vpc_router_dsl;

        dsl::router_route
            .filter(dsl::time_deleted.is_null())
            // ignore built-in services VPC
            .filter(
                dsl::vpc_router_id.ne_all(
                    vpc_router_dsl::vpc_router
                        .select(vpc_router_dsl::id)
                        .filter(vpc_router_dsl::vpc_id.eq(*SERVICES_VPC_ID))
                        .filter(vpc_router_dsl::time_deleted.is_null()),
                ),
            )
            .filter(
                dsl::kind
                    .eq(RouterRouteKind(external::RouterRouteKind::VpcSubnet)),
            )
            .select(RouterRoute::as_select())
            .load_async(&*datastore.pool_connection_for_tests().await.unwrap())
            .await
            .unwrap()
            .len()
            == 1
    }

    async fn one_subnet_exists(datastore: &DataStore, vpc_id: Uuid) -> bool {
        use nexus_db_queries::db::model::VpcSubnet;
        use nexus_db_schema::schema::vpc_subnet::dsl;

        dsl::vpc_subnet
            .filter(dsl::time_deleted.is_null())
            .filter(dsl::vpc_id.eq(vpc_id))
            .select(VpcSubnet::as_select())
            .load_async(&*datastore.pool_connection_for_tests().await.unwrap())
            .await
            .unwrap()
            .len()
            == 1
    }

    #[nexus_test(server = crate::Server)]
    async fn test_saga_basic_usage_succeeds(
        cptestctx: &ControlPlaneTestContext,
    ) {
        let client = &cptestctx.external_client;
        let nexus = &cptestctx.server.server_context().nexus;
        let project_id = create_org_and_project(&client).await;
        let opctx = test_opctx(&cptestctx);

        let (authz_vpc, db_vpc, authz_system_router) =
            get_vpc_state(&cptestctx, project_id).await;
        verify_clean_slate(nexus.datastore(), authz_vpc.id()).await;
        let params =
            new_test_params(&opctx, authz_vpc, db_vpc, authz_system_router);
        nexus.sagas.saga_execute::<SagaVpcSubnetCreate>(params).await.unwrap();
    }

    #[nexus_test(server = crate::Server)]
    async fn test_action_failure_can_unwind(
        cptestctx: &ControlPlaneTestContext,
    ) {
        let log = &cptestctx.logctx.log;

        let client = &cptestctx.external_client;
        let nexus = &cptestctx.server.server_context().nexus;
        let project_id = create_org_and_project(&client).await;
        let (authz_vpc, ..) = get_vpc_state(&cptestctx, project_id).await;
        let vpc_id = authz_vpc.id();

        let opctx = test_opctx(&cptestctx);
        test_helpers::action_failure_can_unwind::<SagaVpcSubnetCreate, _, _>(
            nexus,
            || {
                Box::pin(async {
                    let (authz_vpc, db_vpc, authz_system_router) =
                        get_vpc_state(&cptestctx, project_id).await;
                    new_test_params(
                        &opctx,
                        authz_vpc,
                        db_vpc,
                        authz_system_router,
                    )
                })
            },
            || {
                Box::pin(async {
                    verify_clean_slate(nexus.datastore(), vpc_id).await;
                })
            },
            log,
        )
        .await;
    }

    #[nexus_test(server = crate::Server)]
    async fn test_action_failure_can_unwind_idempotently(
        cptestctx: &ControlPlaneTestContext,
    ) {
        let log = &cptestctx.logctx.log;

        let client = &cptestctx.external_client;
        let nexus = &cptestctx.server.server_context().nexus;
        let project_id = create_org_and_project(&client).await;
        let (authz_vpc, ..) = get_vpc_state(&cptestctx, project_id).await;
        let vpc_id = authz_vpc.id();

        let opctx = test_opctx(&cptestctx);
        test_helpers::action_failure_can_unwind_idempotently::<
            SagaVpcSubnetCreate,
            _,
            _,
        >(
            nexus,
            || {
                Box::pin(async {
                    let (authz_vpc, db_vpc, authz_system_router) =
                        get_vpc_state(&cptestctx, project_id).await;
                    new_test_params(
                        &opctx,
                        authz_vpc,
                        db_vpc,
                        authz_system_router,
                    )
                })
            },
            || {
                Box::pin(async {
                    verify_clean_slate(nexus.datastore(), vpc_id).await;
                })
            },
            log,
        )
        .await;
    }

    #[nexus_test(server = crate::Server)]
    async fn test_actions_succeed_idempotently(
        cptestctx: &ControlPlaneTestContext,
    ) {
        let client = &cptestctx.external_client;
        let nexus = &cptestctx.server.server_context().nexus;
        let project_id = create_org_and_project(&client).await;
        let opctx = test_opctx(&cptestctx);

        let (authz_vpc, db_vpc, authz_system_router) =
            get_vpc_state(&cptestctx, project_id).await;
        verify_clean_slate(nexus.datastore(), authz_vpc.id()).await;
        let params =
            new_test_params(&opctx, authz_vpc, db_vpc, authz_system_router);
        let dag = create_saga_dag::<SagaVpcSubnetCreate>(params).unwrap();
        test_helpers::actions_succeed_idempotently(nexus, dag).await;
    }
}
