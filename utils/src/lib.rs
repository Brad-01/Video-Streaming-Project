use rustls::{Certificate, PrivateKey};
use rustls_pemfile;
use std::error::Error;
use tokio::sync::broadcast;

/// ---------- QUIC Setup Functions ----------
pub async fn load_certs(cert_pem: &[u8]) -> Result<Vec<Certificate>, Box<dyn Error>> {
    let certs = rustls_pemfile::certs(&mut &cert_pem[..])
        .map(|certs| certs.into_iter().map(Certificate).collect::<Vec<_>>())?;
    Ok(certs)
}

pub async fn load_private_keys(key_pem: &[u8]) -> Result<Vec<PrivateKey>, Box<dyn Error>> {
    let keys = rustls_pemfile::pkcs8_private_keys(&mut &key_pem[..])
        .map(|keys| keys.into_iter().map(PrivateKey).collect::<Vec<_>>())?;
    Ok(keys)
}
/// ---------- Commands for protocols ----------
#[derive(Debug, Clone, Copy)]
pub enum ProtoCommands {
    Hello = 33,
    ChooseTvChannel = 34,
    ChannelList = 0,
    InvalidChannel = 1,
    Connected = 2,
    Upload = 3,
    FailedUpload = 4,
}

impl ProtoCommands {
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            33 => Some(ProtoCommands::Hello),
            34 => Some(ProtoCommands::ChooseTvChannel),
            0 => Some(ProtoCommands::ChannelList),
            1 => Some(ProtoCommands::InvalidChannel),
            2 => Some(ProtoCommands::Connected),
            3 => Some(ProtoCommands::Upload),
            _ => None,
        }
    }
}

/// ---------- Channel struct for broadcasting ----------
#[derive(Clone)]
pub struct Channel {
    pub video_sender: broadcast::Sender<Vec<u8>>,
    pub chat_sender: broadcast::Sender<String>,
}
