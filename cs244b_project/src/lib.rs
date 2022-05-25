mod blockchain;
mod messages;
mod network;
mod utils;

use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;

use rand::Rng;
use std::time::Duration;
use tokio::{
    io::{stdin, AsyncBufReadExt, BufReader},
    select, spawn,
    sync::mpsc,
    time::sleep,
};
use log::info;

pub use blockchain::{Block, Chain, LocalChain, BlockchainManager};
pub use messages::{Message, MessageKind, MessagePayload};
pub use network::peer_init;
pub use network::NetworkStack;
pub use utils::crypto::*;

pub struct StreamletInstance {
    pub id: u32,
    pub name: String,
    pub is_leader: bool,
    expected_peer_count: usize,
    blockchain_manager: BlockchainManager,
    keypair: Keypair,
    public_keys: Vec<PublicKey>,
}
enum EventType {
    UserInput(String),
    NetworkInput(Vec<u8>),
    DoInit,
}

// ==========================
// === Core Streamlet API ===
// ==========================

impl StreamletInstance {
    /* Initializer:
        @param id: id number identifying the node (used for leader election)
        @param expected_peer_count: expected number of StreamletInstances running
        @param my_name: identifying "name" of this node */
    pub fn new(id: u32, expected_peer_count: usize, name: String) -> Self {
        // Setup public/private key pair
        let mut csprng = OsRng {};
        let keypair: Keypair = Keypair::generate(&mut csprng);
        let pk: PublicKey = keypair.public.clone();

        // Build the streamlet instance
        Self {
            id: id,
            expected_peer_count: expected_peer_count,
            is_leader: false,
            name: name,
            keypair: keypair,
            blockchain_manager: BlockchainManager::new(),
            public_keys: Vec::from([pk]),
        }
    }

    /* Main straemlet event loop.
        1. Intializes networking stack + input channels (e.g. stdin)
        2. Performs peer discovery
        3. Runs the main event loop */
    pub async fn run(&mut self) {
        // Initialize
        // (1) message queue for the network to send us data
        // (2) message queue for us to receive data from the network
        let (net_sender, mut receiver) = mpsc::unbounded_channel();

        // Initialize the network stack
        let mut net_stack = network::NetworkStack::new("test_messages", net_sender).await;

        // Set up stdin
        let mut stdin = BufReader::new(stdin()).lines();

        // Set up what we need to initialize the peer discovery protocol
        let mut peers = peer_init::Peers::new(self.name.clone(), self.keypair.public);
        let (trigger_init, mut recv_init) = mpsc::channel(1);
        let mut needs_init = true;
        spawn(async move {
            // trigger after MDNS has had a chance to do its thing
            sleep(Duration::from_secs(1)).await;
            trigger_init.send(0).await.expect("can't send init event");
        });

        // Main event loop!
        loop {
            let evt = {
                select! {
                    // User input
                    line = stdin.next_line() => {
                        let line_data = line.expect("Can't get line").expect("Can't read from stdin");
                        Some(EventType::UserInput(line_data))
                    },

                    // When the network receives *any* message, it forwards the data to us thru this channel
                    network_response = receiver.recv() => {
                        Some(EventType::NetworkInput(network_response.expect("Response doesn't exist.")))
                    },

                    // One way to model the initialization event
                    _init = recv_init.recv() => {
                        recv_init.close();
                        if needs_init {
                            needs_init = false;
                            Some(EventType::DoInit)
                        } else {
                            None
                        }
                    },

                    // Needs to be polled in order to make progress.
                    _ = net_stack.clear_unhandled_event() => {
                        None
                    },

                }
            };
            if let Some(event) = evt {
                match event {
                    EventType::UserInput(line) => {
                        if line.starts_with("end discovery") || line.starts_with("e d") {
                            peers.send_end_init(&mut net_stack);
                        } else {
                            println!("User input!");

                            let rand: u32 = rand::thread_rng().gen();
                            let message = Message {
                                payload: MessagePayload::String(line),
                                kind: MessageKind::Test,
                                nonce: rand,
                                signatures: None,
                            };

                            info!("Sending message {:?}", message);

                            net_stack.broadcast_message(message.serialize());
                        }
                    }
                    EventType::NetworkInput(bytes) => {
                        let message = Message::deserialize(&bytes);
                        info!("Received message: {:?}", message);
                        
                        // Message Processing Logic
                        match message.payload {
                            MessagePayload::PeerAdvertisement(ad) => {
                                self.add_public_key(&ad.public_key);
                                peers.recv_advertisement(ad, &mut net_stack);
                            }
                            _ => {}
                        };

                    }
                    EventType::DoInit => {
                        peers.start_init(&mut net_stack, self.expected_peer_count);
                    }
                }
            }
        }
    }

    pub fn get_public_key(&self) -> PublicKey {
        return self.keypair.public;
    }
}

// =========================
// === Streamlet Helpers ===
// =========================

impl StreamletInstance {
    /* Signs an arbitrary slice of bytes
        @param bytes: arbitrary bytes to sign
        Note: should get rid of this? mainly for testing */
    fn sign(&self, bytes: &[u8]) -> Signature {
        return self.keypair.sign(bytes);
    }

    /* Signs a message's payload and adds the signature to the message
        @param message: the message instance with a payload to be signed */
    fn sign_message(&self, message: &mut Message) {
        match &message.signatures {
            Some(_) => {
                let signature: Signature =
                    self.keypair.sign(message.serialize_payload().as_slice());
                let signatures: &mut Vec<Signature> = message.signatures.as_mut().unwrap();
                signatures.push(signature)
            }
            None => panic!("Tried to add signature to message without signature vector!"),
        }
    }

    /* Verifies a (message, signature) pair against a public key.
        @param message: the message instance with a (signature, payload) pair to be validated
        @param signature: signature of the message to be validated
        @param pk: public key to verify against the signature */
    fn verify_signature(&self, message: &Message, signature: &Signature, pk: &PublicKey) -> bool {
        let result = pk.verify(message.serialize_payload().as_slice(), signature);
        if let Err(error) = result {
            return false;
        } else {
            return true;
        }
    }

    /* Verifies message signatures against all known public keys, returning the number of valid signatures
        @param message: the message instance with signatures to be validated */
    fn verify_message(&self, message: &Message) -> usize {
        let mut num_valid_signatures = 0;
        let signatures = message.signatures.as_ref().unwrap(); // Check all signatures
        
        // Check all sigatures on the message
        for signature in signatures.iter() {
            // Check against all known pk's
            for pk in self.public_keys.iter() {
                if self.verify_signature(message, signature, pk) {
                    num_valid_signatures += 1;
                    break;
                }
            }
        }
        return num_valid_signatures;
    }

    /* Determines if the block associated with a message is notarized.
        @param epoch: epoch number */
    pub fn is_notarized(&self, message: &Message) -> bool {
        return self.verify_message(message) >= self.expected_peer_count / 2;
    }

    /* Determines epoch leader using deterministic hash function.
        @param epoch: epoch number */
    fn get_epoch_leader(&self, epoch: i64) -> u32 {
        let mut hasher = DefaultHasher::new();
        hasher.write_i64(epoch);
        let result = hasher.finish() as u32;
        return result % (self.expected_peer_count as u32); // Assumes 0 indexing!
    }
    /* Determines epoch leader using deterministic hash function.
        @param epoch: epoch number
        Note: for testing, should be taken care of in peer discovery. */
    pub fn add_public_key(&mut self, pk: &PublicKey) {
        self.public_keys.push(pk.clone());
    }
}

// ============================
// === Streamlet Unit Tests ===
// ============================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_streamlet_signatures() {
        let streamlet = StreamletInstance::new(0, 1, String::from("Test"));
        // Testing signatures
        let message: &[u8] = b"This is a test of the tsunami alert system.";
        let signature: Signature = streamlet.sign(message);
        let public_key: PublicKey = streamlet.get_public_key();
        assert!(public_key.verify(message, &signature).is_ok());
    }

    #[test]
    fn test_streamlet_msg_signatures() {
        let mut streamlet1 = StreamletInstance::new(0, 3, String::from("Test1"));
        let streamlet2 = StreamletInstance::new(1, 3, String::from("Test2"));
        let streamlet3 = StreamletInstance::new(3, 3, String::from("Test3"));

        // Create random hash
        let mut hasher = Sha256::new();
        hasher.update(b"hello world");
        let result = hasher.finalize();
        let bytes: Sha256Hash = result
            .as_slice()
            .try_into()
            .expect("slice with incorrect length");

        // Create a test block
        let blk = Block::new(0, bytes, String::from("test"), 0, 0);

        // Create a message
        let mut message = Message {
            payload: MessagePayload::Block(blk),
            kind: MessageKind::Vote,
            nonce: 0,
            signatures: Some(Vec::new()),
        };

        // Signing message
        streamlet1.sign_message(&mut message);
        assert!(message.signatures.as_ref().unwrap().len() == 1);
        streamlet2.sign_message(&mut message);
        assert!(message.signatures.as_ref().unwrap().len() == 2);
        streamlet3.sign_message(&mut message);
        assert!(message.signatures.as_ref().unwrap().len() == 3);

        // Adding public keys to streamlet1
        streamlet1.add_public_key(&streamlet2.get_public_key());
        let bad_result = streamlet1.verify_message(&message);
        assert!(bad_result == 2);
        streamlet1.add_public_key(&streamlet3.get_public_key());
        assert!(streamlet1.public_keys.len() == 3);

        // Verify message with all signatures
        let good_result = streamlet1.verify_message(&message);
        assert!(good_result == 3);
    }
}
