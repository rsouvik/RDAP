extern crate syn;
//extern crate hyper;
extern crate futures;
extern crate once_cell;
extern crate anyhow;

use libp2p::gossipsub::{
    IdentTopic as Topic, MessageAuthenticity, ValidationMode,
};

use futures::stream::StreamExt;
use libp2p::{gossipsub, mdns, noise, swarm::NetworkBehaviour, swarm::SwarmEvent, tcp, yamux};
use libp2p::{Swarm, SwarmBuilder};
//use libp2p::{gossipsub::Topic};
use libp2p::request_response::{self, OutboundRequestId, ProtocolSupport, ResponseChannel};
use libp2p::{PeerId};
use libp2p::StreamProtocol;
use std::collections::hash_map::DefaultHasher;
use std::error::Error;
use std::hash::{Hash, Hasher};
use std::time::Duration;
use tokio::{fs, io, io::AsyncBufReadExt, select, sync::mpsc};
use tracing_subscriber::EnvFilter;
use serde::{Serialize,Deserialize};
use std::collections::HashSet;
use tracing::error;
use tracing::log::info;
use syn::synom::Parser;
//use lazy_static::lazy::Lazy;
use once_cell::sync::Lazy;
use lazy_static::lazy_static;
use std::{fs::File, io::Write, process::exit};
use futures::{io, AsyncBufReadExt};
use anyhow::Ok;

use anyhow::Result;
type resources = Vec<ResourceReq>;

#[derive(Debug, Serialize, Deserialize)]
struct Resource {
    id: usize,
    name: String,
    parameters: String,
    instructions: String,
    public: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ResourceReq {
    id: usize,
    //name: String,
    parameters: String, //s3 bucket or ipfs loc
    //instructions: String,
    public: bool,
}

/* Resources */
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ResourceRes {
    id: usize,
    hardware: String,
    power: usize,
    speed: usize,
}

impl ResourceReq {
    fn new(id: usize, parameters: String, public: bool) -> Self {
        Self { id,parameters,public }
    }
}

#[derive(Debug, Serialize, Deserialize)]
enum ListMode {
    ALL,
    One(String),
    ResourceSpecs(String),     //Sent by Verifier
    ResourceSpecsReq(String),  //Prover-initiated
    ResourceConfReq(String),   //Verifier initiated
    Resource(ResourceReq),     //Prover response
}

#[derive(Debug, Serialize, Deserialize)]
struct ListRequest {
    mode: ListMode,
}

#[derive(Debug, Serialize, Deserialize)]
struct ListResponse {
    mode: ListMode,
    data: resources,
    receiver: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ListResponseRequest {
    mode: ListMode,
    data: i32,
    receiver: String,
}

#[derive(Debug)]
enum EventType {
    Response(ListResponse),
    Input(String),
    //Gossipsub(GossipsubEvent),
    //Mdns(MdnsEvent),
}

// We create a custom network behaviour that combines Gossipsub, Mdns and Request-Response.
#[derive(NetworkBehaviour)]
struct MyBehaviour {
    gossipsub: gossipsub::Behaviour,
    mdns: mdns::tokio::Behaviour,
    request_response: request_response::cbor::Behaviour<ListRequest, ListResponse>,
    //#[behaviour(ignore)]
    //response_sender: mpsc::UnboundedSender<ListResponse>,
}

/* Run a metadata server using libp2p */
#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .try_init();

    //let (response_sender, mut response_rcv) = mpsc::unbounded_channel::<(ListResponse)>();
    let (response_sender, mut response_rcv) = mpsc::channel::<(ListResponse)>(10);
    //let (response_sender, mut response_rcv) = mpsc::channel(10);


    let mut swarm = libp2p::SwarmBuilder::with_new_identity()
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_quic()
        .with_behaviour(|key| {
            // To content-address message, we can take the hash of message and use it as an ID.
            let message_id_fn = |message: &gossipsub::Message| {
                let mut s = DefaultHasher::new();
                message.data.hash(&mut s);
                gossipsub::MessageId::from(s.finish().to_string())
            };

            // Set a custom gossipsub configuration
            let gossipsub_config = gossipsub::ConfigBuilder::default()
                .heartbeat_interval(Duration::from_secs(10)) // This is set to aid debugging by not cluttering the log space
                .validation_mode(gossipsub::ValidationMode::Strict) // This sets the kind of message validation. The default is Strict (enforce message signing)
                .message_id_fn(message_id_fn) // content-address messages. No two messages of the same content will be propagated.
                .build()
                .map_err(|msg| io::Error::new(io::ErrorKind::Other, msg))?; // Temporary hack because `build` does not return a proper `std::error::Error`.

            // build a gossipsub network behaviour
            let gossipsub = gossipsub::Behaviour::new(
                gossipsub::MessageAuthenticity::Signed(key.clone()),
                gossipsub_config,
            )?;

            let mdns =
                //mdns::tokio::Behaviour::new(mdns::Config::default(), PEER_ID)?;
                mdns::tokio::Behaviour::new(mdns::Config::default(), key.public().to_peer_id())?;

            let request_response = request_response::cbor::Behaviour::new(
                [(
                    StreamProtocol::new("/resource-exchange/1"),
                    ProtocolSupport::Full,
                )],
                request_response::Config::default(),
            );

            Ok(MyBehaviour { gossipsub, mdns, request_response })
            //Ok(ModelBehaviour { gossipsub, mdns, response_sender })
        })?
        .build();

    // Create a Gossipsub topic
    let topic = gossipsub::IdentTopic::new("test-net");
    //lazy_static! {
    //static topic: &'static Topic = &gossipsub::IdentTopic::new("test-net");
    //}

    // subscribes to our topic
    swarm.behaviour_mut().gossipsub.subscribe(&topic)?;

    // Read full lines from stdin
    let mut stdin = io::BufReader::new(io::stdin()).lines();

    // Listen on all interfaces and whatever port the OS assigns
    swarm.listen_on("/ip4/0.0.0.0/udp/0/quic-v1".parse()?)?;
    swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()?)?;

    println!("Enter messages via STDIN and they will be sent to connected peers using Gossipsub");

    loop {
        select! {
            Ok(Some(line)) = stdin.next_line() => {
                /*if let Err(e) = swarm
                    .behaviour_mut().gossipsub
                    .publish(topic.clone(), line.as_bytes()) {
                    println!("Publish error: {e:?}");
                }*/
                match line.as_str() {
                "ls p" => handle_list_peers(&mut swarm).await,
                 cmd if cmd.starts_with("ls r") => handle_list_models(cmd, &mut swarm ).await,
                _ => {error!("unknown $command"); info!("Command is {:?}",line.as_str());}
                }
            }
            _ => ()
            }
    }
}
