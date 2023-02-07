use crate::node::default_node_name;
use crate::util::{api, node_rpc, Rpc, RpcBuilder};
use crate::{help, node::HELP_DETAIL, CommandGlobalOpts};
use clap::Args;
use colorful::Colorful;
use core::time::Duration;
use ockam::TcpTransport;
use ockam_api::nodes::models::identity::ShortIdentityResponse;
use ockam_api::nodes::models::portal::{InletList, OutletList};
use ockam_api::nodes::models::services::ServiceList;
use ockam_api::nodes::models::transport::TransportList;
use ockam_api::{addr_to_multiaddr, cli_state, route_to_multiaddr};
use ockam_core::Route;
use ockam_multiaddr::proto::{DnsAddr, Node, Tcp};
use ockam_multiaddr::MultiAddr;
use std::marker::PhantomData;
use tokio_retry::strategy::FibonacciBackoff;
use tracing::debug;

const IS_NODE_UP_MAX_ATTEMPTS: usize = 50;
const IS_NODE_UP_MAX_TIMEOUT: Duration = Duration::from_secs(1);

/// Show node details
#[derive(Clone, Debug, Args)]
#[command(arg_required_else_help = true, after_long_help = help::template(HELP_DETAIL))]
pub struct ShowCommand {
    /// Name of the node.
    #[arg(default_value_t = default_node_name())]
    node_name: String,
}

impl ShowCommand {
    pub fn run(self, options: CommandGlobalOpts) {
        node_rpc(run_impl, (options, self))
    }
}

async fn run_impl(
    ctx: ockam::Context,
    (opts, cmd): (CommandGlobalOpts, ShowCommand),
) -> crate::Result<()> {
    let node_name = &cmd.node_name;

    let tcp = TcpTransport::create(&ctx).await?;
    let mut rpc = RpcBuilder::new(&ctx, &opts, node_name).tcp(&tcp)?.build();
    print_query_status(&mut rpc, node_name, false).await?;
    Ok(())
}

pub async fn print_query_status(
    rpc: &mut Rpc<'_>,
    node_name: &str,
    wait_until_ready: bool,
) -> anyhow::Result<()> {
    let cli_state = cli_state::CliState::new()?;
    let node_state = cli_state.nodes.get(node_name)?;
    let node_port = node_state.setup()?.default_tcp_listener()?.addr.port();
    let mut node_info = PrintNodeInfo::init(node_port, node_name);

    if !is_node_up(rpc, wait_until_ready).await? {
        node_info.print()
    } else {
        // Get short id for the node
        rpc.request(api::short_identity()).await?;
        let default_id = match rpc.parse_response::<ShortIdentityResponse>() {
            Ok(resp) => String::from(resp.identity_id),
            Err(_) => String::from("None"),
        };

        node_info.default_id = Some(&default_id);

        // Get list of services for the node
        let mut rpc = rpc.clone();
        rpc.request(api::list_services()).await?;
        let services = rpc.parse_response::<ServiceList>()?;
        node_info.services = Some(&services);

        // Get list of TCP listeners for node
        let mut rpc = rpc.clone();
        rpc.request(api::list_tcp_listeners()).await?;
        let tcp_listeners = rpc.parse_response::<TransportList>()?;
        node_info.tcp_listeners = Some(&tcp_listeners);

        // Get list of Secure Channel Listeners
        let mut rpc = rpc.clone();
        rpc.request(api::list_secure_channel_listener()).await?;
        let secure_channel_listeners = rpc.parse_response::<Vec<String>>()?;
        node_info.secure_channel_listeners = Some(&secure_channel_listeners);

        // Get list of inlets
        let mut rpc = rpc.clone();
        rpc.request(api::list_inlets()).await?;
        let inlets = rpc.parse_response::<InletList>()?;

        // Get list of outlets
        let mut rpc = rpc.clone();
        rpc.request(api::list_outlets()).await?;
        let outlets = rpc.parse_response::<OutletList>()?;

        node_info.inlets_outlets = Some((&inlets, &outlets));

        node_info.is_default = {
            if let Ok(default_node) = cli_state.nodes.default() {
                default_node == node_state
            } else {
                false
            }
        };

        node_info.print();
    }

    Ok(())
}

/// Send message(s) to a node to determine if it is 'up' and
/// responding to requests.
///
/// If `wait_until_ready` is `true` and the node does not
/// appear to be 'up', retry the test at time intervals up to
/// a maximum number of retries. A use case for this is to
/// allow a node time to start up and become ready.
async fn is_node_up(rpc: &mut Rpc<'_>, wait_until_ready: bool) -> anyhow::Result<bool> {
    let attempts = match wait_until_ready {
        true => IS_NODE_UP_MAX_ATTEMPTS,
        false => 1,
    };

    let timeout = FibonacciBackoff::from_millis(250)
        .max_delay(IS_NODE_UP_MAX_TIMEOUT)
        .take(attempts);

    let cli_state = cli_state::CliState::new()?;
    let now = std::time::Instant::now();
    for t in timeout {
        // The node is down if it has not stored its default tcp listener in its state file.
        if cli_state
            .nodes
            .get(rpc.node_name())?
            .setup()?
            .default_tcp_listener()
            .is_err()
        {
            tokio::time::sleep(t).await;
            continue;
        }

        // Test if node is up
        // If node is down, we expect it won't reply and the timeout
        // will trigger the next loop (i.e. no need to sleep here).
        if rpc
            .request_with_timeout(api::query_status(), t)
            .await
            .is_ok()
            && rpc.is_ok().is_ok()
        {
            debug!("Node is up. Took {:?}", now.elapsed());
            return Ok(true);
        }
    }
    Ok(false)
}

#[derive(Clone, Debug)]
struct PrintNodeInfo<'a, State = Initialized> {
    node_port: u16,
    node_name: &'a str,
    is_default: bool,
    status_is_up: bool,
    default_id: Option<&'a str>,
    services: Option<&'a ServiceList<'a>>,
    tcp_listeners: Option<&'a TransportList<'a>>,
    secure_channel_listeners: Option<&'a Vec<String>>,
    inlets_outlets: Option<(&'a InletList<'a>, &'a OutletList<'a>)>,
    phantom_data: PhantomData<State>,
}

impl<'a> PrintNodeInfo<'a, NotInitialized> {
    fn init(node_port: u16, node_name: &'a str) -> PrintNodeInfo<Initialized> {
        PrintNodeInfo::<Initialized> {
            node_port,
            node_name,
            is_default: false,
            status_is_up: false,
            default_id: None,
            services: None,
            tcp_listeners: None,
            secure_channel_listeners: None,
            inlets_outlets: None,
            phantom_data: PhantomData,
        }
    }
}

impl<'a> PrintNodeInfo<'a, Initialized> {
    fn print(&self) {
        let Self {
            node_name,
            node_port,
            is_default,
            status_is_up,
            default_id,
            services,
            tcp_listeners,
            secure_channel_listeners,
            inlets_outlets,
            ..
        } = *self;

        let name = if is_default {
            "{node_name} (default)"
        } else {
            node_name
        };

        println!();
        println!("Node:");
        println!("  Name: {}", name);
        println!(
            "  Status: {}",
            match status_is_up {
                true => "UP".light_green(),
                false => "DOWN".light_red(),
            }
        );

        println!("  Route To Node:");
        let mut m = MultiAddr::default();
        if m.push_back(Node::new(node_name)).is_ok() {
            println!("    Short: {}", m);
        }

        let mut m = MultiAddr::default();
        if m.push_back(DnsAddr::new("localhost")).is_ok()
            && m.push_back(Tcp::new(node_port)).is_ok()
        {
            println!("    Verbose: {}", m);
        }

        if let Some(id) = default_id {
            println!("  Identity: {}", id);
        }

        if let Some(list) = tcp_listeners {
            println!("  Transports:");
            for e in &list.list {
                println!("    Transport:");
                println!("      Type: {}", e.tt);
                println!("      Mode: {}", e.tm);
                println!("      Address: {}", e.payload);
            }
        }

        if let Some(list) = secure_channel_listeners {
            println!("  Secure Channel Listeners:");
            for e in list {
                println!("    Listener:");
                if let Some(ma) = addr_to_multiaddr(e) {
                    println!("      Address: {}", ma);
                }
            }
        }

        if let Some((inlets, outlets)) = inlets_outlets {
            println!("  Inlets:");
            for e in &inlets.list {
                println!("    Inlet:");
                println!("      Listen Address: {}", e.bind_addr);
                if let Some(r) = Route::parse(e.outlet_route.as_ref()) {
                    if let Some(ma) = route_to_multiaddr(&r) {
                        println!("      Route To Outlet: {}", ma);
                    }
                }
            }
            println!("  Outlets:");
            for e in &outlets.list {
                println!("    Outlet:");
                println!("      Forward Address: {}", e.tcp_addr);

                if let Some(ma) = addr_to_multiaddr(e.worker_addr.as_ref()) {
                    println!("      Address: {}", ma);
                }
            }
        }

        if let Some(list) = services {
            println!("  Services:");
            for e in &list.list {
                println!("    Service:");
                println!("      Type: {}", e.service_type);
                if let Some(ma) = addr_to_multiaddr(e.addr.as_ref()) {
                    println!("      Address: {}", ma);
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
struct Initialized;

#[derive(Clone, Debug)]
struct NotInitialized;
