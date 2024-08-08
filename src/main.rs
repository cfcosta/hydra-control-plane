use anyhow::{Context, Result};
use model::{
    hydra::{
        hydra_message::{HydraData, HydraEventMessage},
        state::HydraNodesState,
    },
    node::Node,
};
use rocket::http::Method;
use rocket_cors::{AllowedOrigins, CorsOptions};
use routes::global::global;
use routes::head::head;
use routes::heads::heads;
use routes::new_game::new_game;
use serde::Deserialize;
use std::path::PathBuf;
use tokio::{
    spawn,
    sync::mpsc::{self, UnboundedReceiver, UnboundedSender},
};

#[macro_use]
extern crate rocket;

mod model;
mod routes;

// this is a temporary way to store the script address
pub const SCRIPT_ADDRESS: &str = "addr_test1wp3z9emuaqukk57zsrcnhx0fv2pp9n73cyq7s32mutwklfqjp53s0";
pub const SCRIPT_CBOR: &str = "59054901000032323232323232232323232323232322322533300d32323232323232325333015301330163754016264a66602c6028602e6ea80044cc00800cdd7180d980c1baa00116301a301b301b30173754028264a66602c6024602e6ea80204c8c8c94ccc064c05cc068dd500089919299980d980c980e1baa001132533301c3015301d37540022646464a66603e603a60406ea80044c94ccc080c8c8c8c8c8c8c8c8c8c8c94ccc0ad4ccc0accdc42400060586ea8c0c002c5288a99981599981599baf0070024a0944528899981599baf0090044a09445280991929998169815800899299981718160008a511533302e302a00114a22940c0b8dd50010a9998169814800899299981718138008a511533302e302a00114a22940c0b8dd5001099299981718138008a511533302e302c00114a22940c0b8dd500118169baa3031302e375400e6060605a6ea8004c0bcc0c0008c0b8004c0b8008c0b0004c0b0c0a0dd500518151815801181480098148011813800981380098111baa01f13300c00d00114a06eb8c090c084dd50008b1811981218101baa01d30150013021301e37540022c604060426042603a6ea8c080c074dd50008b19802004119baf3004301d3754002004603c60366ea8c078c07cc06cdd5180f180d9baa00116323300300923375e600660386ea8004008c074c068dd50051180e80091191980080080191299980e8008a60103d87a800013232533301c300500213374a90001981000125eb804cc010010004c084008c07c00458c068c05cdd500591191980080080191299980d8008a5013253330193371e6eb8c078008010528899801801800980f0009bac30183019301930193019301930190023758602e002602e602e0046eb0c054004c044dd5180a0011809980a00098079baa00114984d958c94ccc030c02800454ccc03cc038dd50010a4c2c2a666018601000226464a6660226028004264932999807180618079baa001132323232323232325333019301c002132498cc0400048c94ccc060c05800454ccc06cc068dd50010a4c2c2a66603060280022a66603660346ea80085261616301837540022c6eb0c068004c068008dd6980c000980c0011bad30160013016002375a602800260206ea80045858c048004c038dd50010b18061baa0013001008253330093007300a3754002264646464646464646464a66602c603200426464646493198080021180a000a99980a9809980b1baa0051323232323232533301e302100213232498c064010c94ccc070c06800454ccc07cc078dd50030a4c2c2a66603860300022a66603e603c6ea8018526161533301c30150011533301f301e375400c2930b0b180e1baa00516375a603e002603e004603a002603a0046036002602e6ea801458c03c018c03801c58dd6180b800980b801180a800980a801180980098098011808800980880119299980718068008a999805980398060008a511533300b3009300c00114a02c2c6ea8c03c004c02cdd50008b1b874801088c8cc00400400c894ccc0340045261323300300330110023003300f0012325333007300500113232533300c300f002149858dd7180680098049baa00215333007300300113232533300c300f002149858dd7180680098049baa00216300737540026e1d200225333004300230053754002264646464a666016601c004264932999804180318049baa00313232323232323232323232325333017301a002149858dd6980c000980c0011bad30160013016002375a602800260280046eb4c048004c048008dd6980800098080011bad300e001300a37540062c2c6eb4c030004c030008c028004c018dd50008b1b87480015cd2ab9d5573caae7d5d02ba157441";
struct MyState {
    state: HydraNodesState,
    config: Config,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct Config {
    ttl_minutes: u64,
    nodes: Vec<NodeConfig>,
}

#[derive(Debug, Deserialize)]
struct NodeConfig {
    #[serde(default = "localhost")]
    local_url: String,
    max_players: usize,
    remote_url: Option<String>,
    admin_key_file: PathBuf,
    persisted: bool,
}

fn localhost() -> String {
    "ws://127.0.0.1:4001".to_string()
}

#[rocket::main]
async fn main() -> Result<()> {
    let rocket = rocket::build();
    let figment = rocket.figment();
    let config = figment.extract::<Config>().context("invalid config")?;

    let (tx, rx): (UnboundedSender<HydraData>, UnboundedReceiver<HydraData>) =
        mpsc::unbounded_channel();

    let mut nodes = vec![];
    for node in &config.nodes {
        let node = Node::try_new(&node, &tx)
            .await
            .context("failed to construct new node")?;
        nodes.push(node);
    }

    let hydra_state = HydraNodesState::from_nodes(nodes);

    let hydra_state_clone = hydra_state.clone();
    spawn(async move {
        update(hydra_state_clone, rx).await;
    });

    let cors = CorsOptions::default()
        .allowed_origins(AllowedOrigins::all())
        .allowed_methods(
            vec![Method::Get, Method::Post, Method::Patch]
                .into_iter()
                .map(From::from)
                .collect(),
        )
        .allow_credentials(true);

    let _rocket = rocket::build()
        .manage(MyState {
            state: hydra_state,
            config,
        })
        .mount("/", routes![new_game, heads, head, global])
        .attach(cors.to_cors().unwrap())
        .launch()
        .await?;

    Ok(())
}

async fn update(state: HydraNodesState, mut rx: UnboundedReceiver<HydraData>) {
    loop {
        match rx.recv().await {
            Some(HydraData::Received { message, authority }) => {
                let mut state_guard = state.state.write().await;
                let nodes = &mut state_guard.nodes;
                let node = nodes
                    .iter_mut()
                    .find(|n| n.local_connection.to_authority() == authority);
                if let None = node {
                    warn!("Node not found: ${:?}", authority);
                    continue;
                }
                let node = node.unwrap();
                match message {
                    HydraEventMessage::HeadIsOpen(head_is_open) if node.head_id.is_none() => {
                        info!(
                            "updating node {:?} with head_id {:?}",
                            node.local_connection.to_authority(),
                            head_is_open.head_id
                        );
                        node.head_id = Some(head_is_open.head_id.to_string());
                    }
                    HydraEventMessage::SnapshotConfirmed(snapshot_confirmed) => node
                        .stats
                        .calculate_stats(snapshot_confirmed.confirmed_transactions),

                    HydraEventMessage::TxValid(tx) => match node.add_transaction(tx) {
                        Ok(_) => {}
                        Err(e) => {
                            warn!("failed to add transaction {:?}", e);
                        }
                    },
                    _ => {}
                }
            }
            Some(HydraData::Send(_)) => {}
            None => {
                warn!("mpsc disconnected");
                break;
            }
        }
    }
}
