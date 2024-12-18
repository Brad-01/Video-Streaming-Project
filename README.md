# Desktop/Terminal Based Video Streaming Server
## Project Description
This project is a video streaming server that uses QUIC for communication. 
The server streams multiple channels where each channel allows multiple 
clients to stream video and chat with each other. The server can also
download videos from links provided by the clients.

## Features
- Stream MP4 videos over QUIC.
- Multiple Channels supported.
- Text-based chat between connected clients.
- Ability to upload new content to the server.

# Project Setup 
## Prerequisites
- **Rust & Cargo:** Ensure Rust (stable) is installed.  
  [Install Rust](https://www.rust-lang.org/tools/install)

- **mpv:** Used by the client to play streamed video.  
  [Download mpv](https://github.com/zhongfly/mpv-winbuild/releases)

- **yt-dlp:** Used by the server to download YouTube videos.  
  [Download yt-dlp](https://github.com/yt-dlp/yt-dlp#installation)

- **OpenSSL:** Required to generate certificates.  

Make sure `mpv` and `yt-dlp` are either in the project root or on your system’s PATH.

##  Set UP
1. **QUIC Certificates:**  
   Create a directory `cert` in the project root, then generate certs:
   ```bash
   mkdir cert
   cd cert
   openssl req -x509 -newkey rsa:4096 -keyout server.key -out server.crt -days 365
   ```
   Place server.crt, server.key, and optionally rootCA.crt (if required) in cert/.
2. **Videos**
    Ensure you have server/videos directory at the root of this project. 
   This is where you will place your mp4 videos and where the server will 
   stream and download them too. ***Note:*** There MUST be at least one 
   video for the server to run.


## Running the Server
When you run the server, you can enter the IP address your server will be 
running on to allow outside connections. Otherwise, the server will default 
to 127.0.0.1:8080

Enter the following command to run the server:
- `cargo run --bin server <server_ip>:8080`

## Running the Client
When you run the client, you MUST enter the IP address of the server. 

Enter one of the following command to run the client:
- remote: `cargo run --bin client <server_ip>:8080`
- local: `cargo run --bin client 127.0.0.1:8080`

## How to Use
1. Start the server.
2. Run the client and connect to the server’s IP.
3. Choose a channel by entering a channel number and a username (e.g. 0 alice) 
   or type u to upload a new video from a YouTube link.
4. Once connected, watch the video stream. For chat, type messages into the 
   client terminal; enter q to leave chat.
5. To add new content via a YouTube link, select u and provide the link. The 
   server downloads it and adds it as a new channel.
