use futures_util::{SinkExt, StreamExt};
use futures_util::stream::SplitSink;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305};
use ring::error::Unspecified;
use serde::{Serialize, Deserialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{self, AsyncBufReadExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{accept_async, connect_async, WebSocketStream};
use url::Url;
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};
use tokio_tungstenite::MaybeTlsStream;
use tokio::signal;

#[derive(Serialize, Deserialize, Debug)]
enum ClientMessage {
    Chat { username: String, message: String },
    Typing { username: String, is_typing: bool },
}

#[derive(Serialize, Deserialize, Debug)]
enum ServerMessage {
    Chat { username: String, message: String },
    Typing { username: String, is_typing: bool },
}

type Tx = SplitSink<WebSocketStream<TcpStream>, Message>;
type PeerMap = Arc<Mutex<HashMap<usize, Arc<Mutex<Tx>>>>>;

const SHARED_SECRET: &[u8; 32] = b"an example very very secret key.";
// function for server gang
async fn run_server() -> io::Result<()> {
    let addr = "127.0.0.1:8080";
    let listener = TcpListener::bind(&addr).await.expect("Failed to bind");
    let peer_map: PeerMap = Arc::new(Mutex::new(HashMap::new()));
    let mut id_counter = 0;

    println!("Listening on: {}", addr);

    while let Ok((stream, _)) = listener.accept().await {
        let peer_map = peer_map.clone();
        let id = id_counter;
        id_counter += 1;

        tokio::spawn(async move {
            let ws_stream = match accept_async(stream).await {
                Ok(ws) => ws,
                Err(e) => {
                    eprintln!("Failed to accept WebSocket connection: {:?}", e);
                    return;
                }
            };
            println!("New WebSocket connection: {}", ws_stream.get_ref().peer_addr().unwrap());

            let (write, mut read) = ws_stream.split();
            let write = Arc::new(Mutex::new(write));

            {
                let mut peers = peer_map.lock().await;
                peers.insert(id, write.clone());
            }

            while let Some(Ok(msg)) = read.next().await {
                if msg.is_text() || msg.is_binary() {
                    let peers = {
                        let peers = peer_map.lock().await;
                        peers.clone()
                    };

                    for (&peer_id, peer) in peers.iter() {
                        if peer_id != id {
                            let msg_clone = msg.clone();
                            let peer = peer.clone();
                            tokio::spawn(async move {
                                let mut peer = peer.lock().await;
                                if let Err(e) = peer.send(msg_clone).await {
                                    eprintln!("Failed to send message to peer {}: {}", peer_id, e);
                                }
                            });
                        }
                    }
                }
            }

            {
                let mut peers = peer_map.lock().await;
                peers.remove(&id);
            }

            println!("WebSocket connection {} closed.", id);
        });
    }

    Ok(())
}

// pretty self-explanatory
async fn run_client() -> io::Result<()> {
    println!("Enter your username:");
    let stdin = BufReader::new(io::stdin());
    let mut lines = stdin.lines();

    let username = match lines.next_line().await? {
        Some(line) => line.trim().to_string(),
        None => {
            println!("No username provided.");
            return Ok(());
        }
    };

    let url = Url::parse("ws://127.0.0.1:8080").unwrap();
    let (ws_stream, _) = connect_async(url).await.expect("Failed to connect");
    let (write, read) = ws_stream.split();

    println!("WebSocket connected as '{}'", username);

    let write = Arc::new(Mutex::new(write));

    let incoming_handle = tokio::spawn(handle_incoming_messages(read));

    let write_clone = write.clone();
    let username_clone = username.clone();

    let input_handle = tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            if !line.trim().is_empty() {

                let chat_message = ClientMessage::Chat {
                    username: username_clone.clone(),
                    message: line.clone(),
                };

                let serialized = serde_json::to_string(&chat_message).expect("Failed to serialize message");
                let encrypted_message = encrypt_message(&serialized).expect("Failed to encrypt message");

                let mut write_lock = write_clone.lock().await;
                if let Err(e) = write_lock.send(Message::Binary(encrypted_message)).await {
                    eprintln!("Failed to send message: {:?}", e);
                    break; 
                }
            }
        }
    });

    tokio::select! {
        _ = incoming_handle => {},
        _ = input_handle => {},
        _ = signal::ctrl_c() => {
            println!("Received Ctrl+C, shutting down client...");
        }
    }

    println!("Client exiting.");
    Ok(())
}
// incoming messages ig
async fn handle_incoming_messages(
    mut read: futures_util::stream::SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>
) {

    let mut typing_users: HashMap<String, Instant> = HashMap::new();
    let typing_display_duration = Duration::from_secs(2);

    while let Some(msg) = read.next().await {
        if let Ok(msg) = msg {
            match msg {
                Message::Binary(data) => {
                    match decrypt_message(&data) {
                        Ok(decrypted) => {
                            match serde_json::from_str::<ServerMessage>(&decrypted) {
                                Ok(server_msg) => {
                                    match server_msg {
                                        ServerMessage::Chat { username, message } => {
                                            println!("{}: {}", username, message);
                                        },
                                        ServerMessage::Typing { username, is_typing } => {
                                            if is_typing {
                                                typing_users.insert(username.clone(), Instant::now());
                                            } else {
                                                typing_users.remove(&username);
                                            }

                                            display_typing_indicators(&mut typing_users, typing_display_duration);
                                        },
                                    }
                                }
                                Err(e) => {
                                    eprintln!("Failed to deserialize message: {:?}", e);
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("Failed to decrypt message: {:?}", e);
                        }
                    }
                },
                Message::Text(text) => {
                    println!("Received (Text): {}", text);
                },
                _ => {
                    println!("Received non-text/binary message.");
                }
            }
        }
    }
}
// some cool stuff
fn display_typing_indicators(typing_users: &mut HashMap<String, Instant>, display_duration: Duration) {
    let now = Instant::now();
    let mut active_typing: Vec<String> = Vec::new();

    typing_users.retain(|username, &mut time| {
        if now.duration_since(time) < display_duration {
            active_typing.push(username.clone());
            true
        } else {
            false
        }
    });

    if !active_typing.is_empty() {
        let users = active_typing.join(", ");
        let is_are = if active_typing.len() > 1 { "are" } else { "is" };
        println!("🤔 {} {} typing...", users, is_are);
    }
}
// ooo nice
fn encrypt_message(message: &str) -> Result<Vec<u8>, Unspecified> {
    let key = UnboundKey::new(&CHACHA20_POLY1305, SHARED_SECRET)?;
    let nonce = Nonce::assume_unique_for_key([0; 12]); 
    let less_safe_key = LessSafeKey::new(key);
    let mut in_out = message.as_bytes().to_vec();
    less_safe_key.seal_in_place_append_tag(nonce, Aad::empty(), &mut in_out)?;
    Ok(in_out)
}
// yoooooo
fn decrypt_message(ciphertext: &[u8]) -> Result<String, Unspecified> {
    let key = UnboundKey::new(&CHACHA20_POLY1305, SHARED_SECRET)?;
    let nonce = Nonce::assume_unique_for_key([0; 12]); 
    let less_safe_key = LessSafeKey::new(key);

    let mut ciphertext_vec = ciphertext.to_vec();
    let decrypted = less_safe_key.open_in_place(nonce, Aad::empty(), &mut ciphertext_vec)?;
    Ok(String::from_utf8_lossy(decrypted).to_string())
}
// main sauce lowkey
#[tokio::main]
async fn main() -> io::Result<()> {
    println!("Enter 'server' to run as server or 'client' to run as client:");
    let stdin = BufReader::new(io::stdin());
    let mut lines = stdin.lines();

    let role = match lines.next_line().await? {
        Some(line) => line.trim().to_string(),
        None => {
            println!("No input provided.");
            return Ok(());
        }
    };

    match role.as_str() {
        "server" => run_server().await?,
        "client" => run_client().await?,
        _ => println!("Invalid input, please enter 'server' or 'client'."),
    }
    Ok(())
}