extern crate syn;
//extern crate hyper;
extern crate futures;
extern crate once_cell;
extern crate tensorflow;
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
use aws_sdk_s3 as s3;

use tensorflow::{Graph, SavedModelBundle, SessionOptions, SessionRunArgs, Tensor, Status};
use anyhow::Result;

//mod KTensor;

const STORAGE_FILE_PATH: &str = "./models.json";
const AVAILABILITY_FILE_PATH: &str = "./resources.json";
type models = Vec<ModelReq>;
type resources = Vec<ModelRes>;
//static TOPIC: Lazy<Topic> = Lazy::new(|| Topic::new("karamel"));
//static KEYS: Lazy<identity::Keypair> = Lazy::new(|| identity::Keypair::generate_ed25519());
static TOPIC: Lazy<Topic> = Lazy::new(|| gossipsub::IdentTopic::new("test-net"));
//static PEER_ID: Lazy<PeerId> = Lazy::new(|| PeerId::from(KEYS.public()));
type Result1<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync + 'static>>;

#[derive(Debug, Serialize, Deserialize)]
struct Model {
    id: usize,
    name: String,
    parameters: String,
    instructions: String,
    public: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ModelReq {
    id: usize,
    //name: String,
    parameters: String, //s3 bucket or ipfs loc
    //instructions: String,
    public: bool,
}

/* Model Resources */
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ModelRes {
    id: usize,
    hardware: String,
    power: usize,
    speed: usize,
}

impl ModelReq {
    fn new(id: usize, parameters: String, public: bool) -> Self {
        Self { id,parameters,public }
    }
}

#[derive(Debug, Serialize, Deserialize)]
enum ListMode {
    ALL,
    One(String),
    //MODEL(Vec<f32>),
    ModelSpecs(String),     //Sent by Verifier
    ModelSpecsReq(String),  //Prover-initiated
    ModelConfReq(String),   //Verifier initiated
    Model(ModelReq),        //Prover response
}

#[derive(Debug, Serialize, Deserialize)]
struct ListRequest {
    mode: ListMode,
}

#[derive(Debug, Serialize, Deserialize)]
struct ListResponse {
    mode: ListMode,
    data: models,
    receiver: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ListResponseModel {
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


/*#[derive(NetworkBehaviour)]
#[behaviour(out_event = "EventType")]
struct ModelBehaviour {
    gossipsub: Gossipsub,
    mdns: Mdns,
    #[behaviour(ignore)]
    response_sender: mpsc::UnboundedSender<ListResponse>,
}

impl From<GossipsubEvent> for EventType{
    fn from(event: GossipsubEvent) -> Self {
        EventType::Gossipsub(event)
    }
}

impl From<MdnsEvent> for EventType{
    fn from(event: MdnsEvent) -> Self {
        EventType::Mdns(event)
    }
}*/

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
                    StreamProtocol::new("/model-exchange/1"),
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

    // Kick it off
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
            event = swarm.select_next_some() => match event {
                SwarmEvent::Behaviour(MyBehaviourEvent::Mdns(mdns::Event::Discovered(list))) => {
                    for (peer_id, _multiaddr) in list {
                        println!("mDNS discovered a new peer: {peer_id}");
                        swarm.behaviour_mut().gossipsub.add_explicit_peer(&peer_id);
                    }
                },
                SwarmEvent::Behaviour(MyBehaviourEvent::Mdns(mdns::Event::Expired(list))) => {
                    for (peer_id, _multiaddr) in list {
                        println!("mDNS discover peer has expired: {peer_id}");
                        swarm.behaviour_mut().gossipsub.remove_explicit_peer(&peer_id);
                    }
                },
                //request response
                SwarmEvent::Behaviour(MyBehaviourEvent::RequestResponse(
                request_response::Event::Message { peer, message, .. },
                ))  => match message {
                    request_response::Message::Request {
                        request, channel, ..
                    } => {
                        println!( "Got message {:?} in request_response from : {:?}",request,peer);
                        match request.mode {
                            ListMode::ModelSpecsReq(ref spec_req) => {

                                let mreq = r#"{"id":1,"parameters":"picxelate","public":false}"#;

                                let req =
                                    ListRequest {
                                        mode: ListMode::ModelConfReq(mreq.to_string()),
                                    };
                                    swarm
                                        .behaviour_mut()
                                        .request_response
                                        .send_request(&peer, req);

                            }

                            ListMode::ModelConfReq(ref spec_req) => {

                                if let Ok(mre) = serde_json::from_slice::<ModelReq>(&spec_req.as_bytes()) {
                                    println!( "Location: {:?}",mre.parameters);
                                    match gen_model_response(&mre).await {
                                        Ok(()) => {
                                            println!( "Model generated locally at peer {:?}",swarm.local_peer_id().to_string());
                                        }
                                        Err(e) => error!("error generating model, {}", e),
                                    }
                                }

                            }


                            _ => {}
                        }
                    }
                    request_response::Message::Response {
                        request_id,
                        response,
                    } => {

                    }
                    }




                 /*=> {
                        println!( "Got message {:?} in request_response from : {:?}",message,peer);
                        //Check for req-res events
                        //ModelSpecsReq
                        if let Ok(req) = serde_json::from_slice::<ListRequest>(&message.Request.request) {

                        }
                      }*/
                SwarmEvent::Behaviour(MyBehaviourEvent::Gossipsub(gossipsub::Event::Message {
                    propagation_source: peer_id,
                    message_id: id,
                    message,
                })) =>
                        {

                            println!(
                            "Got message: '{}' with id: {id} from peer: {peer_id}",
                            String::from_utf8_lossy(&message.data));

                            if let Ok(resp) = serde_json::from_slice::<ListResponse>(&message.data) {
                                //if resp.receiver == PEER_ID.to_string() {
                                if resp.receiver == swarm.local_peer_id().to_string() {

                                    info!("Response from {:?}:", peer_id);
                                    resp.data.iter().for_each(|r| info!("{:?}", r));
                                }
                            }
                            else if let Ok(req) = serde_json::from_slice::<ListRequest>(&message.data) {   //deserialize here
                                match req.mode {
                                    ListMode::ALL => {
                                        info!("Received ALL req: {:?} from {:?}", req, message.source);
                                        //respond_with_public_models(&mut swarm, response_sender.clone(),peer_id.to_string());
                                        //respond_with_public_models(&mut swarm, response_sender.clone(),peer_id);

                                    }
                                    ListMode::One(ref peer_id_dest) => {
                                        if *peer_id_dest == swarm.local_peer_id().to_string() {
                                            info!("Received req: {:?} from {:?}", req, message.source);
                                            info!("Sending req: {:?} back to: {:?}", req, message.source);
                                            info!("Sending req: {:?} back to peer: {:?}", req, peer_id.to_string());
                                            /*respond_with_public_models(&mut swarm,
                                                response_sender.clone(),
                                                //msg.source.to_string(),
                                                //peer_id.to_string()
                                                peer_id
                                            );*/
                                            match read_local_resources().await {
                                                Ok(v) => {
                                                    info!("Local resources ({})", v.len());
                                                    v.iter().for_each(|r| info!("{:?}", r));
                                                }
                                                Err(e) => error!("error fetching local resources: {}", e),
                                            };
                                            let req = ListRequest {
                                                    mode: ListMode::ALL,
                                            };
                                            swarm
                                                .behaviour_mut()
                                                .request_response
                                                .send_request(&peer_id, req);
                                        }
                                    }
                                    ListMode::Model(ref modelReq) => {

                                    }
                                    ListMode::ModelSpecs(ref modelReq) => {
                                        //if modelReq == swarm.local_peer_id().capacity()
                                        //then send ModelSpecsReq through reqres
                                        match resources_available_for_request(modelReq).await {

                                            Ok(v) => {
                                                if v == true {
                                                    //send modelspecsreq from prover to verifier
                                                    //let req = ListRequest {
                                                    //mode: ListMode::ALL,
                                                     //};

                                                     let req = ListRequest {
                                                        mode: ListMode::ModelSpecsReq(modelReq.to_owned()),
                                                     };
                                                    swarm
                                                        .behaviour_mut()
                                                        .request_response
                                                        .send_request(&peer_id, req);
                                                }
                                            }
                                            Err(e) => error!("error fetching local resources: {}", e),
                                        }
                                    }
                                    //not part of gossip, move to req-response
                                    ListMode::ModelSpecsReq(ref modelReq) => {

                                    }
                                    _ => ()

                                }
                             }

                /*if let Err(e) = swarm
                    .behaviour_mut().gossipsub
                    .publish(topic.clone(), (String::from_utf8_lossy(&message.data)+"now").as_bytes()) {
                    println!("Publish error: {e:?}");
                }*/

                        },
                SwarmEvent::NewListenAddr { address, .. } => {
                    println!("Local node is listening on {address}");
                }
                _ => {}
            }
        }
    }
}

/* List peers */
async fn handle_list_peers(swarm: &mut Swarm<MyBehaviour>) {
    info!("Discovered Peers:");
    let nodes = swarm.behaviour().mdns.discovered_nodes();
    let mut unique_peers = HashSet::new();
    for peer in nodes {
        unique_peers.insert(peer);
    }
    unique_peers.iter().for_each(|p| info!("{}", p));
}

//Broadcast request to other peers
async fn handle_list_models(cmd: &str, swarm: &mut Swarm<MyBehaviour>) {
    let topic1 = gossipsub::IdentTopic::new("test-net");
    let rest = cmd.strip_prefix("ls r ");
    match rest {
        Some("all") => {
            info!("Command is {:?}", cmd);
            let req = ListRequest {
                mode: ListMode::ALL,
            };
            let json = serde_json::to_string(&req).expect("can jsonify request");

            swarm
                .behaviour_mut()
                .gossipsub
                .publish(topic1.clone(), json.as_bytes());

        }
        /*Some(models_peer_id) => {

            /*let remote_peer_multiaddr: Multiaddr = models_peer_id.parse().unwrap();
            swarm.dial(remote_peer_multiaddr).unwrap();
            println!("Dialed remote peer: {:?}", models_peer_id);*/

            let req = ListRequest {
                mode: ListMode::One(models_peer_id.to_owned()),
            };
            let json = serde_json::to_string(&req).expect("can jsonify request");
            swarm
                .behaviour_mut()
                .gossipsub
                .publish(topic1.clone(), json.as_bytes());
        }*/

        Some(models_resource) => {

            let req = ListRequest {
                mode: ListMode::ModelSpecs(models_resource.to_owned()),
            };
            let json = serde_json::to_string(&req).expect("can jsonify request");
            swarm
                .behaviour_mut()
                .gossipsub
                .publish(topic1.clone(), json.as_bytes());
        }
        /*
        //Add a model request with location and ID, here location is s3 bucket
        Some(parameters) => {
            //let mut mreq = ModelReq::new(1,"location",false);
            let mut mreq = ModelReq {
                id: 1,
                parameters: parameters.to_string(),
                public: false,
            };
            let req = ListRequest {
                mode: ListMode::Model(mreq.to_owned())
            };
            let json = serde_json::to_string(&mreq).expect("can jsonify request");
            swarm
                .behaviour_mut()
                .floodsub
                .publish(TOPIC.clone(), json.as_bytes());
        }*/
        Some(_) => {

        }
        None => {
            info!("Local models empty");
            match read_local_models().await {
                Ok(v) => {
                    info!("Local models ({})", v.len());
                    v.iter().for_each(|r| info!("{:?}", r));
                }
                Err(e) => error!("error fetching local models: {}", e),
            };
        }
    };
}

async fn read_local_models() -> Result1<models> {
    info!("Inside local models");
    let content = fs::read(STORAGE_FILE_PATH).await?;
    let result = serde_json::from_slice(&content)?;
    Ok(result)
}

//for example, { "hardware":"gpu","power":110,"speed":200 }
async fn read_local_resources() -> Result1<resources> {
    info!("Inside local resources");
    let content = fs::read(AVAILABILITY_FILE_PATH).await?;
    let result = serde_json::from_slice(&content)?;
    Ok(result)
}

//fn respond_with_public_models(sender: mpsc::UnboundedSender<ListResponse>, receiver: String) {
fn respond_with_public_models(sender: mpsc::Sender<ListResponse>, receiver: PeerId) {

    tokio::spawn(async move {
        info!("Inside respond_with_public_models0");
        match read_local_models().await {
            Ok(v) => {
                info!("Inside respond_with_public_models");
                /*let resp = ListResponse {
                    mode: ListMode::ALL,
                    receiver,
                    data: v.into_iter().filter(|r| r.public).collect(),
                };*/
                //let _ = sender.send(resp);
                /*if let Err(e) = sender.send(resp).await
                    .expect("Command receiver not to be dropped."); {
                    info!("Error: Inside respond_with_public_models");
                    error!("error sending response via channel, {}", e);
                }*/
            }
            Err(e) => error!("error fetching local models to answer ALL request, {}", e),
        }
    });
}

async fn resources_available_for_request(model_req : &str) -> Result1<bool>{

    let mut rval = true;
    if let Ok(req) = serde_json::from_slice::<ModelRes>(model_req.as_bytes()) {

        match read_local_resources().await {
            Ok(v) => {
                info!("Local resources ({})", v.len());
                v.iter().for_each(|r|
                    {
                        /*for val in r {
                            let (key,v) = val;
                            if key == "speed" {
                                if v < req.speed {
                                    rval = rval & false;
                                }
                            }
                            if key == "power" {
                                if v < req.power {
                                    rval = rval & false;
                                }
                            }
                        }*/
                        if r.speed < req.speed {
                            rval = rval & false;
                        }

                        if r.power < req.power {
                            rval = rval & false;
                        }
                        info!("{:?}", r);
                        //if r.speed < req.speed {rval=false;}
                        //if r.power < req.power {rval=false;}
                    });
            }
            Err(e) => error!("error fetching local resources: {}", e),
        };
    }

    //if rval==false {return false;}

    //return true;
    Ok(rval)
}

/* Download model from some location */
pub async fn downloadModel(m: &ModelReq) {

    let config = aws_config::load_from_env().await;
    let client = &s3::Client::new(&config);

    //let bucket_name = "your-s3-bucket-name";
    //let key = "your/s3/object/key";
    //let file_inp = "file_s3";

    let bucket_name = &m.parameters;
    let key = "dgp/create_model.py";
    let file_inp = "Model/create_model.py";

    let mut file = File::create(file_inp).unwrap();

    let object = client
        .get_object()
        .bucket(bucket_name)
        .key(key)
        .send()
        .await;

    let mut byte_count = 0_usize;
    /*while let Some(bytes) = object.unwrap().body.try_next().await? {
        let bytes = file.write(&bytes)?;
        byte_count += bytes;
        trace!("Intermediate write of {bytes}");
    }*/

    let body = object.unwrap().body.collect().await;
    let data_bytes = body.unwrap().into_bytes();
    let bytes = file.write(&data_bytes).unwrap();

    // do something with the data
    //println!("{:?}", data_bytes);

    //Ok(byte_count)
    //Ok(())

}

//Create model for rust
//run python through bash command
async fn create_model_for_rust() {
    std::process::Command::new("python3")
        .current_dir("./Model")
        .arg("create_model.py")
        .output()
        .expect("failed to execute process");
}

/* generate the model */
//async fn gen_model() -> Result<()> {
async fn gen_model() {

    let train_input_parameter_input_name = "training_input";
    let train_input_parameter_target_name = "training_target";
    let pred_input_parameter_name = "inputs";

    //Names of output nodes of the graph, retrieved with the saved_model_cli command
    let train_output_parameter_name = "output_0";
    let pred_output_parameter_name = "output_0";

    //Create some tensors to feed to the model for training, one as input and one as the target value
    //Note: All tensors must be declared before args!
    let input_tensor: Tensor<f32> = Tensor::new(&[1,2]).with_values(&[1.0, 1.0]).unwrap();
    let target_tensor: Tensor<f32> = Tensor::new(&[1,1]).with_values(&[2.0]).unwrap();

    //Path of the saved model
    let save_dir = "Model/custom_model";

    //Create a graph
    let mut graph = Graph::new();

    //Load save model as graph
    let bundle = SavedModelBundle::load(
        &SessionOptions::new(), &["serve"], &mut graph, save_dir
    ).expect("Can't load saved model");

    //Initiate a session
    let session = &bundle.session;

    //Alternative to saved_model_cli. This will list all signatures in the console when run
    // let sigs = bundle.meta_graph_def().signatures();
    // println!("{:?}", sigs);


    //Retrieve the train functions signature
    let signature_train = bundle.meta_graph_def().get_signature("train").unwrap();

    //Input information
    let input_info_train = signature_train.get_input(train_input_parameter_input_name).unwrap();
    let target_info_train = signature_train.get_input(train_input_parameter_target_name).unwrap();

    //Output information
    let output_info_train = signature_train.get_output(train_output_parameter_name).unwrap();

    //Input operation
    let input_op_train = graph.operation_by_name_required(&input_info_train.name().name).unwrap();
    let target_op_train = graph.operation_by_name_required(&target_info_train.name().name).unwrap();

    //Output operation
    let output_op_train = graph.operation_by_name_required(&output_info_train.name().name).unwrap();

    //The values will be fed to and retrieved from the model with this
    let mut args = SessionRunArgs::new();

    //Feed the tensors into the graph
    args.add_feed(&input_op_train, 0, &input_tensor);
    args.add_feed(&target_op_train, 0, &target_tensor);

    //Fetch result from graph
    let mut out = args.request_fetch(&output_op_train, 0);

    //Run the session
    session
        .run(&mut args)
        .expect("Error occurred during calculations");

    //Retrieve the result of the operation
    let loss: f32 = args.fetch(out).unwrap()[0];

    println!("Loss: {:?}", loss);
    // Ok(())
}

//Generate final model response
async fn gen_model_response(model: &ModelReq) -> Result<()>{

    let dw = downloadModel(model).await;//.expect("Model download not successful!!");
    let cw = create_model_for_rust().await;//.expect("Create model failed!!");
    let gm = gen_model().await;//.expect("Generate model failed!!");

    return Ok(());

}


