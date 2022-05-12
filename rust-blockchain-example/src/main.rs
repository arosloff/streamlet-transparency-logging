/* ANNOTATED COPY OF OPEN SOURCE, LIBP2P BLOCKCHAIN TUTORIAL
   ORIGINAL HERE: https://github.com/zupzup/rust-blockchain-example/blob/main/src/main.rs 
   Proposing that we start with this as a baseline structure, adding our logic into it. */

// TO RUN: RUST_LOG=info cargo run

// Offers a timestamp: Utc::now();
// use chrono::prelude::*;
// Libp2p modules
use libp2p::{
    // For creating transport
    core::upgrade,
    // Creates a future that resolves to the next 
    // item (piece of data) in a stream.
    // Used for `select_next_some`, 
    futures::StreamExt,
    mplex,
    noise::{Keypair, NoiseConfig, X25519Spec},
    swarm::{Swarm, SwarmBuilder},
    tcp::TokioTcpConfig,
    Transport,
};
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::Duration;
use rand::Rng;
use tokio::{
    io::{stdin, AsyncBufReadExt, BufReader},
    select, spawn,
    sync::mpsc,
    time::sleep,
};


mod p2p;
mod blockchain;

use blockchain::Block;

pub struct App {
    pub blocks: Vec<blockchain::Block>,
}


impl App {
    fn new() -> Self {
        Self { blocks: vec![] }
    }

    fn genesis(&mut self) {
        let data = String::from("genesis!");
        let hash = blockchain::hash(&data);
        let genesis_block = Block {
            data: data,
            hash: hash,
            prev_hash: "".to_string()
        };
        self.blocks.push(genesis_block);
    }

    fn try_add_block(&mut self, block: Block) {
        let latest_block = self.blocks.last().expect("there is at least one block");
        if block.validate_block(latest_block) {
            self.blocks.push(block);
        } else {
            error!("could not add block - invalid");
        }
    }

    fn is_chain_valid(&self, chain: &[Block]) -> bool {
        for i in 0..chain.len() {
            if i == 0 {
                continue;
            }
            let first = chain.get(i - 1).expect("has to exist");
            let second = chain.get(i).expect("has to exist");
            if !second.validate_block(first) {
                return false;
            }
        }
        true
    }

    // We always choose the longest valid chain
    fn choose_chain(&mut self, local: Vec<Block>, remote: Vec<Block>) -> Vec<Block> {
        let is_local_valid = self.is_chain_valid(&local);
        let is_remote_valid = self.is_chain_valid(&remote);

        if is_local_valid && is_remote_valid {
            if local.len() >= remote.len() {
                local
            } else {
                remote
            }
        } else if is_remote_valid && !is_local_valid {
            remote
        } else if !is_remote_valid && is_local_valid {
            local
        } else {
            panic!("local and remote chains are both invalid");
        }
    }
}


#[tokio::main]
async fn main() {
    // Logging (can change how we do this!)
    pretty_env_logger::init();

    // Initialize state from provided key data 
    info!("Starting up with peer id: {}", *p2p::PEER_ID); 
    let auth_keys = Keypair::<X25519Spec>::new()
        .into_authentic(&p2p::KEYS)
        .expect("can't create auth keys for p2p channel");

    // Initialize async channels
    // (Reminder: these allow us to create events within the host)
    let (response_sender, mut response_rcv) = mpsc::unbounded_channel();
    let (init_sender, mut init_rcv) = mpsc::unbounded_channel();

    // Set up transport layer with default features
    let transp = TokioTcpConfig::new()  // TCP/IP transport capability 
        // Allows us to "upgrade" the basic transport (returns upgrade::Builder)
        .upgrade(upgrade::Version::V1)
        // Basic transport layer security
        .authenticate(NoiseConfig::xx(auth_keys).into_authenticated())
        // Use one transport for multiple channels (topics)
        .multiplex(mplex::MplexConfig::new())
        .boxed(); // Put it on the heap

    // Initialize the NetworkBehaviour in our p2p library
    // "App" should encapsulate all of our application logic
    // These init and response channels will be used to trigger in-application events b/t async/sync tasks
    let behaviour = p2p::AppBehaviour::new(App::new(), response_sender, init_sender.clone()).await;

    // Key part of libp2p: everything about the state of the network and its behavior
    // We define the transport layer (above) and behavior (in libp2p), then 
    // put it on a tokio async executor. 
    let mut swarm = SwarmBuilder::new(transp, behaviour, *p2p::PEER_ID)
        .executor(Box::new(|fut| {
            tokio::spawn(fut);
        }))
        .build();

    // For user input (will be interpreted as in-app events)
    let mut stdin = BufReader::new(stdin()).lines();

    // Set up local socket to run transport/protocol on 
    // Note: peers will be discovered through mdns
    Swarm::listen_on(
        &mut swarm,
        "/ip4/0.0.0.0/tcp/0" // Let OS choose binding
            .parse()
            .expect("can'tt get a local socket"),
    )
    .expect("swarm can't be started");

    // Initialize the app
    spawn(async move {
        // Wait for setup (e.g., MDNS to discover peers)
        sleep(Duration::from_secs(1)).await;
        info!("sending init event");
        // Send init event on internal init channel
        init_sender.send(true).expect("can't send init event");
    });

    // MAIN EVENT LOOP 
    // Note: swarm events that we care about (i.e., events that come from the network)
    // will be triggered by our network behavior. 
    loop {
        let evt = {
            // Tokio macro -- executor will continuously execute first event to trigger
            select! {
                line = stdin.next_line() => Some(p2p::EventType::Input(line.expect("can get line").expect("can read line from stdin"))),
                // Item is received on this internal channel
                response = response_rcv.recv() => {
                    Some(p2p::EventType::LocalChainResponse(response.expect("response exists")))
                },
                // Item is received on this internal channel
                _init = init_rcv.recv() => {
                    Some(p2p::EventType::Init)
                }
                // Unhandled swarm events (just clear out)
                _event = swarm.select_next_some() => {
                    None
                },
            }
        };

        // Behavior for events that aren't triggered just from receiving something from the network
        if let Some(event) = evt {
            match event {
                p2p::EventType::Init => {
                    do_init(&mut swarm);
                }
                p2p::EventType::LocalChainResponse(resp) => {
                    // Internally-triggered
                    let json = serde_json::to_string(&resp).expect("can't jsonify response");
                    swarm
                        .behaviour_mut()
                        .floodsub
                        .publish(p2p::CHAIN_TOPIC.clone(), json.as_bytes());
                }
                p2p::EventType::Input(line) => match line.as_str() {
                    // Handle user commands
                    "ls p" => p2p::handle_print_peers(&swarm),
                    cmd if cmd.starts_with("ls c") => p2p::handle_print_chain(&swarm),
                    cmd if cmd.starts_with("create b") => p2p::handle_create_block(cmd, &mut swarm),
                    cmd if cmd.starts_with("send m") => p2p::handle_send_msg(cmd, &mut swarm),
                    _ => error!("unknown command"),
                },
            }
        }
    }
}

fn do_init(swarm: &mut Swarm<p2p::AppBehaviour>) {
    let peers = p2p::get_list_peers(&swarm);
    swarm.behaviour_mut().app.genesis();

    info!("connected nodes: {}", peers.len());
    if !peers.is_empty() {
        // Note: could send to all peers here?
        let req = p2p::LocalChainRequest {
            from_peer_id: peers
                .iter()
                .last()
                .expect("No peers!")
                .to_string(),
        };

        // Request a peer's blockchain state to get started
        let json = serde_json::to_string(&req).expect("can't jsonify request");
        swarm
            .behaviour_mut()
            .floodsub
            .publish(p2p::CHAIN_TOPIC.clone(), json.as_bytes());
    }
}