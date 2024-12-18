use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{broadcast, RwLock};
use std::env;
use std::error::Error;
use std::net::SocketAddr;
use tokio::process::Command;
use std::process::Stdio;use std::sync::Arc;
use std::time::Duration;
use quinn::{Endpoint, Connection, TransportConfig};
use tokio::fs;
use utils::{load_certs, load_private_keys, Channel, ProtoCommands};
use std::path::Path;
use bytes::{BufMut, BytesMut};
use tokio::fs as tokio_fs;
use regex::Regex;

// Function to retrieve all MP4 file paths from a given directory
async fn get_mp4_files(directory: &str) -> Result<Vec<String>, Box<dyn Error>> {
    let mut mp4_files = Vec::new();
    let mut entries = fs::read_dir(directory).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("mp4") {
            mp4_files.push(path.to_string_lossy().to_string());
        }
    }
    Ok(mp4_files)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Parse the server address from command-line arguments or default to 127.0.0.1:8080
    // To go across computers, use computer's IP and port then client can just connect to that
    let addr = env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:8080".to_string());
    let addr = addr.parse::<SocketAddr>()?;

    // Retrieve all MP4 files from the server/videos directory
    let mp4_files = get_mp4_files("server/videos").await?;

    if mp4_files.is_empty() {
        return Err("No MP4 files found in server/videos directory".into());
    }

    mp4_files.len();

    // Load TLS certificates and private keys
    let cert_pem = fs::read("cert/server.crt").await?;
    let key_pem = fs::read("cert/server.key").await?;

    let certs = load_certs(&cert_pem).await?;
    let mut keys = load_private_keys(&key_pem).await?;

    if keys.is_empty() {
        return Err("Failed to load private key".into());
    }

    // Configure TLS settings with ALPN protocol "bear-tv"
    let mut tls_config = rustls::ServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth()
        .with_single_cert(certs, keys.remove(0))?;

    tls_config.alpn_protocols = vec![b"bear-tv".to_vec()];

    // Configure QUIC transport settings
    let mut transport_config = TransportConfig::default();
    transport_config.max_idle_timeout(Some(Duration::from_secs(120).try_into()?)); // Set idle timeout
    transport_config.keep_alive_interval(Some(Duration::from_secs(30))); // Set keep-alive interval

    // Initialize Quinn server configuration
    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(tls_config));
    server_config.transport_config(Arc::new(transport_config));

    // Bind the server to the specified address
    let endpoint = Endpoint::server(server_config, addr)?;
    println!("Listening on QUIC: {}", addr);

    // Initialize broadcast channels for each TV channel (video and chat)
    let channels: Arc<RwLock<Vec<Channel>>> = Arc::new(RwLock::new(
        mp4_files
            .iter()
            .map(|mp4_file_path| {
                // Create a broadcast channel for video data
                let (video_tx, _) = broadcast::channel(100);
                // Create a broadcast channel for chat messages
                let (chat_tx, _) = broadcast::channel(100);

                // Spawn a task to continuously stream the MP4 file into the video broadcast channel
                let mp4_file_path = mp4_file_path.clone();
                let video_tx_clone = video_tx.clone();
                tokio::spawn(async move {
                    if let Err(e) =
                        stream_mp4_to_broadcast_channel(mp4_file_path, video_tx_clone).await
                    {
                        eprintln!("Error streaming mp4 to broadcast channel: {}", e);
                    }
                });

                // Return the Channel struct containing both senders
                Channel {
                    video_sender: video_tx,
                    chat_sender: chat_tx,
                }
            })
            .collect(),
    ));

    // Accept incoming QUIC connections
    while let Some(conn) = endpoint.accept().await {
        // Retrieve all MP4 files from the server/videos directory
        let mp4_files = get_mp4_files("server/videos").await?;
        // After retrieving MP4 files
        if mp4_files.is_empty() {
            return Err("No MP4 files found in server/videos directory".into());
        }

        let num_channels = mp4_files.len();

        let channels_clone = channels.clone();
        tokio::spawn(async move {
            match conn.await {
                Ok(connection) => {
                    if let Err(e) =
                        handle_quic_client(connection, channels_clone, num_channels).await
                    {
                        eprintln!("Error processing connection: {}", e);
                    }
                }
                Err(e) => eprintln!("Failed to establish connection: {}", e),
            }
        });
    }
    Ok(())
}

// Function to handle individual QUIC client connections
async fn handle_quic_client(
    conn: Connection,
    channels: Arc<RwLock<Vec<Channel>>>,
    num_channels: usize,
) -> Result<(), Box<dyn Error>> {
    println!("Accepted QUIC connection from: {}", conn.remote_address());

    // Accept an initial bidirectional stream for the handshake
    let (mut send, mut recv) = conn.accept_bi().await?;
    handle_stream(&mut send, &mut recv, conn.clone(), channels, num_channels).await?;

    Ok(())
}

// Main function to handle the initial handshake and channel selection
async fn handle_stream(
    send: &mut quinn::SendStream,
    recv: &mut quinn::RecvStream,
    conn: Connection,
    channels: Arc<RwLock<Vec<Channel>>>,
    num_channels: usize,
) -> Result<(), Box<dyn Error>> {
    let mut buf = [0u8; 3];

    // Read the initial Hello message from the client
    recv.read_exact(&mut buf).await?;
    println!("Received bytes from client: {:?}", buf);

    if buf[0] == ProtoCommands::Hello as u8 {
        println!("Received Hello from client.");

        // Send the list of available channels to the client
        let mut response = Vec::new();
        response.push(ProtoCommands::ChannelList as u8);
        response.extend_from_slice(&(num_channels as u16).to_le_bytes());
        send.write_all(&response).await?;

        // Read the client's channel choice
        recv.read_exact(&mut buf).await?;
        if buf[0] == ProtoCommands::ChooseTvChannel as u8 {
            let chosen_channel = u16::from_le_bytes([buf[1], buf[2]]);

            if (chosen_channel as usize) < num_channels {
                // Valid channel selection; send Connected response
                let mut response = vec![ProtoCommands::Connected as u8];
                response.extend_from_slice(&chosen_channel.to_le_bytes());
                send.write_all(&response).await?;
                println!("Client connected to channel: {}", chosen_channel);

                // Acquire a read lock on the channels
                let channels_read = channels.read().await;
                let video_rx = channels_read[chosen_channel as usize]
                    .video_sender
                    .subscribe();
                let chat_sender_clone = channels_read[chosen_channel as usize].chat_sender.clone();

                // Open a new unidirectional stream for video data
                let video_send_stream = conn.open_uni().await?;
                // Spawn the video streaming task
                tokio::spawn(async move {
                    if let Err(e) = handle_video_streaming(video_send_stream, video_rx).await {
                        eprintln!("Error in video streaming: {}", e);
                    }
                });

                // Open a new bidirectional stream for chat messages
                let (chat_send_stream, chat_recv_stream) = conn.accept_bi().await?;
                // Handle chat messages
                tokio::spawn(async move {
                    if let Err(e) = handle_chat_messages(
                        chat_send_stream,
                        chat_recv_stream,
                        chat_sender_clone,
                    )
                        .await
                    {
                        eprintln!("Error in chat handling: {}", e);
                    }
                });
            } else {
                // Invalid channel selection; notify the client
                let invalid_channel_response = vec![ProtoCommands::InvalidChannel as u8];
                send.write_all(&invalid_channel_response).await?;
                println!(
                    "Client attempted to connect to an invalid channel: {}",
                    chosen_channel
                );
            }
        } else if buf[0] == ProtoCommands::Upload as u8 {
            // Read the YouTube link from the client
            let mut youtube_link = Vec::new();

            if buf[1] != 0 {
                youtube_link.push(buf[1]);
                if buf[2] != 0 {
                    youtube_link.push(buf[2]);
                }
            }


            let mut new_buf = [0u8; 1024];
            recv.read(&mut new_buf).await.expect("reading yt link error");
            youtube_link.extend_from_slice(&new_buf);

            let youtube_link = match String::from_utf8(youtube_link) {
                Ok(link) => link.trim_matches('\0').to_string(),
                Err(e) => {
                    eprintln!("Invalid UTF-8: {}", e);
                    return Err(Box::new(e));
                }
            };

            if youtube_link.is_empty() {
                let error_response = vec![ProtoCommands::FailedUpload as u8];
                send.write_all(&error_response).await?;
                println!("Received invalid YouTube link.");
            } else {
                println!("Received YouTube link: {}", youtube_link);

                handle_upload_and_create_channels(&youtube_link, &channels).await?;

                // Send success response and updated channel list
                let mut upload_message = BytesMut::with_capacity(1);
                upload_message.put_u8(ProtoCommands::Upload as u8);
                send.write_all(&upload_message).await.expect("server write upload");

                tokio::time::sleep(Duration::from_millis(5)).await;

                println!("Upload command processed successfully for link: {}", youtube_link);
            }
        } else {
            println!("Received invalid command for choosing channel.");
        }
    }

    Ok(())
}


// Function to handle video streaming to the client over a unidirectional stream
async fn handle_video_streaming(
    mut send: quinn::SendStream,
    mut video_rx: broadcast::Receiver<Vec<u8>>,
) -> Result<(), Box<dyn Error>> {
    println!("Started video streaming task.");
    loop {
        match video_rx.recv().await {
            Ok(data) => {
                if let Err(e) = send.write_all(&data).await {
                    eprintln!("Error sending video data to client: {}", e);
                    break;
                }
                if let Err(e) = send.flush().await {
                    eprintln!("Error flushing video data to client: {}", e);
                    break;
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {
                eprintln!("Client lagged behind video stream.");
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => {
                eprintln!("Video broadcast channel closed.");
                break;
            }
        }
    }
    Ok(())
}

// Function to handle chat messages (both sending and receiving) over a bidirectional stream
async fn handle_chat_messages(
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    chat_sender: broadcast::Sender<String>,
) -> Result<(), Box<dyn Error>> {
    let mut chat_rx = chat_sender.subscribe();
    let chat_sender_clone = chat_sender.clone();

    let mut len_buf = [0u8; 2];

    loop {
        tokio::select! {
            // Receive chat messages from the client
            result = recv.read_exact(&mut len_buf) => {
                match result {
                    Ok(_) => {
                        let msg_len = u16::from_le_bytes(len_buf);
                        let mut msg_buf = vec![0u8; msg_len as usize];
                        if let Err(e) = recv.read_exact(&mut msg_buf).await {
                            eprintln!("Error reading chat message: {}", e);
                            break;
                        }
                        let msg = match String::from_utf8(msg_buf) {
                            Ok(m) => m,
                            Err(e) => {
                                eprintln!("Invalid UTF-8 message received: {}", e);
                                continue;
                            }
                        };
                        // Broadcast the received chat message to all subscribers in the same channel
                        if let Err(e) = chat_sender_clone.send(msg) {
                            eprintln!("Error broadcasting chat message: {}", e);
                        }
                    }
                    Err(e) => {
                        eprintln!("Error reading chat message length: {}", e);
                        break;
                    }
                }
            },
            // Send broadcasted chat messages to the client
            result = chat_rx.recv() => {
                match result {
                    Ok(msg) => {
                        let msg_bytes = msg.as_bytes();
                        let msg_len = msg_bytes.len() as u16;
                        if let Err(e) = send.write_all(&msg_len.to_le_bytes()).await {
                            eprintln!("Error sending chat message length: {}", e);
                            break;
                        }
                        if let Err(e) = send.write_all(msg_bytes).await {
                            eprintln!("Error sending chat message: {}", e);
                            break;
                        }
                        if let Err(e) = send.flush().await {
                            eprintln!("Error flushing chat message stream: {}", e);
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(count)) => {
                        eprintln!("Client lagged behind by {} chat messages", count);
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        eprintln!("Chat broadcast channel closed.");
                        break;
                    }
                }
            }
        }
    }
    Ok(())
}

// Function to continuously stream an MP4 file into the video broadcast channel
async fn stream_mp4_to_broadcast_channel(
    mp4_file_path: String,
    tx: broadcast::Sender<Vec<u8>>,
) -> Result<(), Box<dyn Error>> {
    loop {
        // Spawn ffmpeg to convert MP4 to MPEG-TS on the fly
        let mut child = Command::new("ffmpeg")
            .arg("-re")           // read input at native frame rate
            .arg("-i").arg(&mp4_file_path)  // input file
            .arg("-c").arg("copy")          // no re-encoding
            .arg("-f").arg("mpegts")        // output format as MPEG-TS
            .arg("pipe:1")                  // output to stdout
            .stdout(Stdio::piped())
            .stderr(Stdio::null())          // optional: silence ffmpeg logs
            .spawn()?;

        let mut stdout = child.stdout.take().ok_or("Failed to take stdout")?;
        let mut buf = [0u8; 1024];

        // Read data from ffmpeg stdout and broadcast it
        loop {
            let n = stdout.read(&mut buf).await?;
            if n == 0 {
                // ffmpeg ended or reached EOF
                break;
            }
            let data = buf[..n].to_vec();
            let _ = tx.send(data); // broadcast the MPEG-TS data
        }

        // Wait for ffmpeg to exit before looping again
        let _ = child.wait().await?;
    }
}


async fn get_next_user_filename(directory: &str, base_name: &str) -> Result<String, Box<dyn Error>> {
    // List all files in the directory
    let mut files = tokio_fs::read_dir(directory).await?;

    // Regex to match filenames of the format 'user_<number>'
    let re = Regex::new(r"^user_(\d+)\.mp4$").unwrap();
    let mut existing_numbers = Vec::new();

    // Collect all numbers from existing filenames
    while let Some(entry) = files.next_entry().await? {
        let path = entry.path();
        if path.is_file() {
            if let Some(filename) = path.file_name().and_then(|name| name.to_str()) {
                if let Some(caps) = re.captures(filename) {
                    if let Some(num) = caps.get(1) {
                        existing_numbers.push(num.as_str().parse::<u32>()?);
                    }
                }
            }
        }
    }

    // Find the next available number (i.e., the smallest missing number)
    let mut next_number = 0;
    while existing_numbers.contains(&next_number) {
        next_number += 1;
    }

    // Construct the new filename
    let new_filename = format!("{base_name}_{next_number}.mp4");

    Ok(new_filename)
}

async fn download_and_process_youtube_link(youtube_link: &str) -> Result<String, Box<dyn Error>> {
    // Define the output directory
    let output_directory = "./server/videos";

    // Ensure the videos directory exists
    tokio_fs::create_dir_all(output_directory).await?;

    // Step 1: Use yt-dlp to download the video
    let base_filename = "user"; // Base filename for the video (you can customize this)
    let new_filename = get_next_user_filename(output_directory, base_filename).await?;

    // Full path for the output video
    let output_path = format!("{}/{}", output_directory, new_filename);

    // Step 2: Use yt-dlp to download the YouTube video
    let yt_dlp_command = match Command::new("./yt-dlp")
        .arg("-o")
        .arg(&output_path)  // Use the new filename
        .arg(youtube_link)
        .arg("-f")
        .arg("mp4")
        .arg("--quiet")
        .arg("--no-warnings")
        .output()
        .await
    {
        Ok(output) => output,
        Err(_) => Command::new("yt-dlp")
            .arg("-o")
            .arg(&output_path)
            .arg(youtube_link)
            .arg("-f")
            .arg("mp4")
            .arg("--quiet")
            .arg("--no-warnings")
            .output()
            .await?,
    };

    if !yt_dlp_command.status.success() {
        return Err("Failed to download video".into());
    }


    // Step 3: If needed, convert the video to MP4 using ffmpeg (if not already in MP4 format)
    let video_path = Path::new(&output_path);
    if !video_path.exists() {
        return Err("Downloaded video file does not exist".into());
    }

    // Convert `Path` to `String`
    let video_path_string = video_path.to_string_lossy().to_string();

    Ok(video_path_string)
}



async fn handle_upload_and_create_channels(
    youtube_link: &str,
    channels: &Arc<RwLock<Vec<Channel>>>,
) -> Result<(), Box<dyn Error>> {
    // Step 1: Download and process the YouTube link
    let video_path = download_and_process_youtube_link(youtube_link).await?;

    // Step 2: Create new broadcast channels for the new video
    let (video_tx, _) = broadcast::channel(100);
    let (chat_tx, _) = broadcast::channel(100);

    // Stream the video to the video broadcast channel
    let video_path_clone = video_path.clone();
    let video_tx_clone = video_tx.clone();
    tokio::spawn(async move {
        if let Err(e) = stream_mp4_to_broadcast_channel(video_path_clone, video_tx_clone).await {
            eprintln!("Error streaming new video: {}", e);
        }
    });

    // Step 3: Add the new channel to the shared `channels` list
    let new_channel = Channel {
        video_sender: video_tx,
        chat_sender: chat_tx,
    };

    {
        let mut channels_lock = channels.write().await;
        channels_lock.push(new_channel);
        // Explicitly drop the lock to release it earlier.
        drop(channels_lock);
    }

    Ok(())
}