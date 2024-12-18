use tokio::io::AsyncWriteExt;
use bytes::{BufMut, BytesMut};
use quinn::{Endpoint, RecvStream, SendStream, TransportConfig};
use rustls:: RootCertStore;
use std::error::Error;
use std::fmt::Debug;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{self, AsyncBufReadExt};
use tokio::sync::mpsc;
use tokio::fs;
use utils::{load_certs, ProtoCommands};
use tokio::process::{Child, Command};
use std::process::Stdio;
use tokio::sync::mpsc::Receiver;

#[derive(Debug)]
pub struct QuicChannels {
    pub send: SendStream,
    pub recv: RecvStream,
    pub connection: quinn::Connection,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Parse command-line arguments
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        eprintln!("Usage: {} <server_address:port>", args[0]);
        std::process::exit(1);
    }

    let server_addr: SocketAddr = args[1].parse()?;

    // Create a channel for user input
    let (user_input_tx, mut user_input_rx) = mpsc::channel::<String>(10);
    tokio::spawn(async move {
        let stdin = io::BufReader::new(io::stdin());
        let mut lines = stdin.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if user_input_tx.send(line).await.is_err() {
                println!("Failed to send line");
                break;
            }
        }
    });

    let mut run_bool = true;

    println!("Initiating QUIC connection...");
    let mut quic_channels = init_quic(server_addr).await?;
    while run_bool {
        run_bool = user_ops(&mut quic_channels, &mut user_input_rx).await?;
        if run_bool {
            println!("Restarting QUIC connection...");
            // Re-initialize the QUIC connection
            quic_channels = init_quic(server_addr).await?;
        } else {
            println!("QUIC connection terminated.");
        }
    }

    Ok(())
}

async fn init_quic(
    server_addr: SocketAddr,
) -> Result<QuicChannels, Box<dyn Error>> {
    // Load the server certificate
    let cert_pem = fs::read("cert/rootCA.crt").await?;
    let certs = load_certs(&cert_pem).await?;

    let mut root_cert_store = RootCertStore::empty();
    for cert in certs {
        root_cert_store.add(&cert)?;
    }

    // Configure client to use the loaded root certificate
    let mut tls_config = rustls::ClientConfig::builder()
        .with_safe_defaults()
        .with_root_certificates(root_cert_store)
        .with_no_client_auth();

    // Set ALPN protocols
    tls_config.alpn_protocols = vec![b"bear-tv".to_vec()];

    // Create the transport configuration
    let mut transport_config = TransportConfig::default();
    transport_config.max_idle_timeout(Some(Duration::from_secs(120).try_into()?)); // No idle timeout
    transport_config.keep_alive_interval(Some(Duration::from_secs(30))); // Keep-alive every 30 seconds

    // Create quinn::ClientConfig with the rustls::ClientConfig
    let mut client_config = quinn::ClientConfig::new(Arc::new(tls_config));
    // Attach the transport config
    client_config.transport_config(Arc::new(transport_config));

    // Create a client endpoint
    let mut endpoint = Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(client_config);

    // Connect to the server
    let connection = endpoint.connect(server_addr, "localhost")?.await?;
    println!("Connected to server");

    // Open a bidirectional stream to the server
    let (mut send, recv) = connection.open_bi().await?;

    // Send Hello message
    let mut hello_message = BytesMut::with_capacity(3);
    hello_message.put_u8(ProtoCommands::Hello as u8);
    hello_message.put_u16_le(0); // We can ignore UDP port
    send.write_all(&hello_message).await?;

    Ok(QuicChannels {
        send,
        recv,
        connection
    })
}

async fn user_ops(
    quic_channels: &mut QuicChannels,
    user_input_rx: &mut Receiver<String>
) -> Result<bool, Box<dyn Error>> {
    // Receive ChannelList response
    let mut buf = [0u8; 3];
    quic_channels.recv.read_exact(&mut buf).await?;

    let num_channels = if buf[0] == ProtoCommands::ChannelList as u8 {
        let count = u16::from_le_bytes([buf[1], buf[2]]);
        println!(
            "Welcome to BU-Tube! There are {} TV stations available.",
            count
        );
        count
    } else {
        println!("Unexpected message from server");
        return Ok(true);
    };

    // Handle user input for channel selection
    let username = handle_channel_selection(&mut quic_channels.send, &mut quic_channels.recv, num_channels, user_input_rx).await?;

    if username == "u" {
        Ok(true)
    } else {

        // Accept the unidirectional stream for video data
        let mut video_recv_stream = quic_channels.connection.accept_uni().await?;
        let video_task = tokio::spawn(async move {
            if let Err(e) = handle_video_stream(&mut video_recv_stream).await {
                eprintln!("Error receiving video data: {}", e);
            }
        });

        // Open a new bidirectional stream for chat messages
        let (mut chat_send_stream, mut chat_recv_stream) = quic_channels.connection.open_bi().await?;
        let username_owned = username.clone();

        // Spawn task to receive chat messages
        let chat_recv_task = tokio::spawn(async move {
            if let Err(e) = receive_chat_messages(&mut chat_recv_stream).await {
                eprintln!("Error receiving chat messages: {}", e);
            }
        });

        // Handle sending chat messages within user_ops
        if let Err(e) = send_chat_messages(&mut chat_send_stream, user_input_rx, username_owned).await {
            eprintln!("Error sending chat messages: {}", e);
        }

        // Wait for other tasks to complete
        tokio::try_join!(video_task, chat_recv_task)?;

        Ok(true)
    }
}

fn spawn_mpv(executable: &str) -> io::Result<Child> {
    Command::new(executable)
        .args(&[
            "--cache=yes",
            "--loop",
            "-",
            "--geometry=50%x50%",
            "--msg-level=all=no", // Suppress all log messages
        ])
        .stdin(Stdio::piped())
        .spawn()
}

async fn handle_video_stream(
    recv: &mut RecvStream,
) -> Result<bool, Box<dyn Error>> {
    // Spawn the video player process (e.g., mpv)
    let mut child = spawn_mpv("mpv").or_else(|_| spawn_mpv("./mpv"))
        .expect("Failed to spawn mpv");

    let mut video_player_stdin = child.stdin.take().expect("Failed to open stdin");

    println!("Video player (mpv) started. Streaming video...");

    let mut buf = [0u8; 4096];

    loop {
        match recv.read(&mut buf).await {
            Ok(Some(n)) => {
                // Write the received data to the video player's stdin
                if let Err(_e) = video_player_stdin.write_all(&buf[..n]).await {
                    println!("Video player (mpv) exited.");
                    println!("Send 'q' to leave chat.");
                    break;
                }
            }
            Ok(None) => {
                println!("Video stream closed by server.");
                break;
            }
            Err(e) => {
                eprintln!("Error reading video stream: {}", e);
                break;
            }
        }

        // Check if the video player process is still running
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    eprintln!("Video player (mpv) exited with an error: {}", status);
                    println!("Send 'q' to leave chat.");
                } else {
                    println!("Video player (mpv) exited successfully.");
                    println!("Send 'q' to leave chat.");
                }
                break; // Exit the loop if the player has closed
            }
            Ok(None) => {
                // The player is still running, continue the loop
            }
            Err(e) => {
                eprintln!("Error checking video player status: {}", e);
                break;
            }
        }
    }

    Ok(true)
}


// Handle receiving chat messages from the server
async fn receive_chat_messages(
    recv: &mut RecvStream,
) -> Result<(), Box<dyn Error>> {
    loop {
        // Read the message length (u16)
        let mut len_buf = [0u8; 2];
        match recv.read_exact(&mut len_buf).await {
            Ok(_) => (),
            Err(e) => {
                eprintln!("Error reading message length: {}", e);
                break;
            }
        }
        let msg_len = u16::from_le_bytes(len_buf);

        // Read the message
        let mut msg_buf = vec![0u8; msg_len as usize];
        match recv.read_exact(&mut msg_buf).await {
            Ok(_) => (),
            Err(e) => {
                eprintln!("Error reading message: {}", e);
                break;
            }
        }

        let msg = String::from_utf8(msg_buf)?;
        println!("{}", msg);
    }
    Ok(())
}

// Handle sending chat messages to the server
async fn send_chat_messages(
    send: &mut SendStream,
    user_input_rx: &mut Receiver<String>,
    username: String,
) -> Result<(), Box<dyn Error>> {
    // Send chat connection message
    let conn_message = format!("{} {}", username, "has entered the chat!");

    // Prepend message length (u16)
    let conn_msg_len = conn_message.len() as u16;
    let mut conn_message_bytes = BytesMut::with_capacity(2 + conn_msg_len as usize);
    conn_message_bytes.put_u16_le(conn_msg_len);
    conn_message_bytes.extend_from_slice(conn_message.as_bytes());

    send.write_all(&conn_message_bytes).await?;
    send.flush().await?;

    while let Some(input) = user_input_rx.recv().await {
        let message = format!("{}: {}", username, input.trim());

        // Check for quit command
        if input.trim().eq_ignore_ascii_case("q") {
            println!("Exiting chat...");
            send.finish().await?; // Gracefully finish the stream when quitting
            break;
        }

        // Prepend message length (u16)
        let msg_len = message.len() as u16;
        let mut message_bytes = BytesMut::with_capacity(2 + msg_len as usize);
        message_bytes.put_u16_le(msg_len);
        message_bytes.extend_from_slice(message.as_bytes());

        // Send the message
        send.write_all(&message_bytes).await?;
        send.flush().await?;
    }
    Ok(())
}

async fn handle_channel_selection(
    send: &mut SendStream,
    recv: &mut RecvStream,
    num_channels: u16,
    user_input_rx: &mut Receiver<String>,
) -> Result<String, Box<dyn Error>> {
    let current_num_channels = num_channels;

    loop {
        println!("To upload a YouTube video, enter 'u'.\n\
        To exit the program, enter 'exit'\n\
        Select a channel by entering (0-{}) username (e.g. '0 alice')",
                 current_num_channels - 1
        );


        match user_input_rx.recv().await {
            Some(input) => {
                let input = input.trim();
                match input.to_ascii_lowercase().as_str() {
                    "exit" => {
                        println!("Exiting...");
                        std::process::exit(0);
                    }
                    "u" => {
                        // Handle the upload command
                        match handle_upload(send, recv, user_input_rx).await {
                            Ok(ProtoCommands::Upload) => {
                                return Ok("u".to_string());
                            }
                            Err(e) => {
                                println!("Error: {}", e);
                                return Ok("u".to_string());
                            }
                            _ => {return Ok("u".to_string());}
                        }
                    }
                    _ => {
                        // Handle channel selection
                        match handle_channel_choice(send, recv, current_num_channels, input).await {
                            Ok(username) => return Ok(username),
                            Err(e) => {
                                println!("Error: {}", e);
                                continue;
                            }
                        }
                    }
                }
            }
            None => {
                println!("User input channel closed. Exiting.");
                std::process::exit(0);
            }
        }

    }
}

async fn handle_upload(
    send: &mut SendStream,
    recv: &mut RecvStream,
    user_input_rx: &mut Receiver<String>,
) -> Result<ProtoCommands, Box<dyn Error>> {
    println!("Please enter the YouTube link to upload:");
    if let Some(youtube_link) = user_input_rx.recv().await {
        let youtube_link = youtube_link.trim();
        if youtube_link.is_empty() {
            println!("You must provide a valid YouTube link.");
            return Ok(ProtoCommands::FailedUpload);
        }

        // Send the upload command with the YouTube link
        send_upload_command(send, youtube_link).await.expect("Send upload command");

        // Receive the server's response to the upload request
        let mut response_buf = [0u8; 1];
        recv.read_exact(&mut response_buf).await?;
        let response_code: u8 = response_buf[0];

        match ProtoCommands::from_u8(response_code) {
            Some(ProtoCommands::Upload) => {
                println!("Upload successful.");
                return Ok(ProtoCommands::Upload);
            }
            Some(ProtoCommands::FailedUpload) => {
                println!("Upload failed.");
                return Ok(ProtoCommands::FailedUpload);
            }
            _ => {
                println!("Unexpected response from server during upload.");
            }
        }
    }

    Ok(ProtoCommands::FailedUpload)
}

async fn handle_channel_choice(
    send: &mut SendStream,
    recv: &mut RecvStream,
    num_channels: u16,
    input: &str,
) -> Result<String, Box<dyn Error>> {
    // Split the input into the channel number and username
    let mut parts = input.splitn(2, ' ');
    if let (Some(channel_part), Some(username)) = (parts.next(), parts.next()) {
        match channel_part.parse::<u16>() {
            Ok(channel_number) => {
                if channel_number >= num_channels {
                    return Err(format!(
                        "Channel number is out of range (0-{})",
                        num_channels - 1
                    )
                        .into());
                }

                send_choose_tv_channel(send, channel_number).await?;

                // Receive server response
                let mut response_buf = [0u8; 3];
                recv.read_exact(&mut response_buf).await?;
                let response_code: u8 = response_buf[0];

                match ProtoCommands::from_u8(response_code) {
                    Some(ProtoCommands::Connected) => {
                        println!("Connected to TV Channel {}", channel_number);
                        return Ok(username.to_string());
                    }
                    Some(ProtoCommands::InvalidChannel) => {
                        return Err("Invalid channel selected.".into());
                    }
                    _ => {
                        return Err(format!(
                            "Unexpected response from server: {}",
                            response_code
                        )
                            .into());
                    }
                }
            }
            Err(_) => {
                return Err(format!("'{}' is not a valid number", channel_part).into());
            }
        }
    } else {
        return Err(
            "Input must include a channel number and a username (e.g., '0 alice')".into(),
        );
    }
}

async fn send_choose_tv_channel(
    send: &mut SendStream,
    channel_number: u16,
) -> Result<(), Box<dyn Error>> {
    let mut message = BytesMut::with_capacity(3);
    message.put_u8(ProtoCommands::ChooseTvChannel as u8);
    message.put_u16_le(channel_number);
    send.write_all(&message).await.expect("Send choose tv channel");
    Ok(())
}

async fn send_upload_command(
    send: &mut SendStream,
    youtube_link: &str,
) -> Result<(), Box<dyn Error>> {
    // Send the upload command along with the YouTube link to the server
    let upload_command = ProtoCommands::Upload as u8;
    send.write_all(&[upload_command]).await?;
    send.write_all(youtube_link.as_bytes()).await?;
    Ok(())
}
