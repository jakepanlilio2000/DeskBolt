use thiserror::Error;

/// Unified protocol, framing, codec, and cryptographic error type.
#[derive(Debug, Error)]
pub enum ProtoError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Frame length {len} exceeds maximum {max} bytes")]
    FrameTooLarge { len: usize, max: usize },

    #[error("Decompressed payload length {len} exceeds maximum {max} bytes")]
    DecompressedTooLarge { len: usize, max: usize },

    #[error("Unknown protocol channel id: 0x{0:02x}")]
    UnknownChannel(u8),

    #[error("Invalid reserved frame flag bits: 0x{0:02x}")]
    InvalidFrameFlags(u8),

    #[error("Serialization or codec error: {0}")]
    Codec(String),

    #[error("TLS error: {0}")]
    Tls(String),

    #[error("Host certificate fingerprint requires trust confirmation: {display}")]
    TofuConfirmationRequired {
        fingerprint: [u8; 32],
        display: String,
    },

    #[error("Host certificate fingerprint mismatch (expected {expected}, got {observed})")]
    TofuFingerprintMismatch { expected: String, observed: String },

    #[error("Authentication error: {0}")]
    Auth(String),

    #[error("Rate limited; retry after {retry_after_secs} seconds")]
    RateLimited { retry_after_secs: u16 },

    #[error("Invalid input: {0}")]
    InvalidInput(String),
}
