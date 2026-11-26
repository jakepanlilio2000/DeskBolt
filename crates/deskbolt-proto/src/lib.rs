pub mod auth;
pub mod codec;
pub mod cursor;
pub mod error;
pub mod frame;
pub mod messages;
#[cfg(not(target_os = "android"))]
pub mod native_clipboard;
pub mod tls;
pub mod voice;

pub use auth::{
    ARGON2_M_KIB, ARGON2_P_COST, ARGON2_T_COST, IpRateLimiter, NONCE_LEN, SALT_LEN,
    StoredCredential, compute_client_proof, compute_server_proof, derive_auth_key, random_bytes,
    verify_client_proof, verify_server_proof,
};
pub use codec::{
    H264VideoDecoder, H264VideoEncoder, TileRect, apply_move_rect_rgba, apply_tile_batch_rgba,
    bgra_to_rgba_into, encode_dirty_tile,
};
pub use error::ProtoError;
pub use cursor::{CURSOR_INVERT_PIXEL, CursorDelta, CursorShape, CursorSnapshot, MAX_CURSOR_SIDE};
pub use frame::{
    AsyncFrameReader, ChannelId, FLAG_COMPRESSED, FLAG_END_OF_STREAM, FLAG_KEYFRAME, FLAG_NONE, FLAG_URGENT, Frame,
    HEADER_LEN, MAX_DECOMPRESSED_PAYLOAD, MAX_FRAME_PAYLOAD, bincode_options, decode_bincode,
    encode_bincode, pack_message, read_frame_async, read_frame_sync, unpack_message,
    write_frame_async, write_frame_sync,
};
pub use messages::{
    ClipboardMsg, ClipboardSync, CommsMsg, ControlMsg, DEFAULT_CLIPBOARD_MAX_BYTES, DESKTOP_UPLOAD_PREFIX, DesktopState,
    DirtyTile, DiskStat, DriveEntry, FileEntry, FileTransferMsg, InputMsg, IpcMsg, KeyEvent,
    MAX_CLIPBOARD_BYTES, MonitorInfo, MouseBtn, MoveRect, PointerEvent,
    PowerAction, PowerMsg, ProcessStat, SerialParity, ServiceOp, ServiceStat, ServiceStateKind,
    SessionMode, SpecialAction, SyncEntry, SysAdminMsg, SystemSnapshot, TerminalMsg, TransferDir,
    TunnelMsg, VideoMsg, VideoQualityMode, valid_drop_filename,
};
pub use tls::{
    ClientTlsStream, DEFAULT_PORT, GeneratedIdentity, PROTO_VERSION, ServerTlsStream,
    TlsServerConfig, TofuPinPolicy, TofuServerCertVerifier, accept_tls, build_client_config,
    build_server_config, connect_tls, ensure_ring_provider, format_fingerprint,
    generate_self_signed_cert, load_identity_from_pem_bytes, parse_fingerprint, sha256_fingerprint,
};
pub use voice::{
    VOICE_FRAME_SAMPLES, VOICE_SAMPLE_RATE, VoiceControls, VoiceEngine, list_voice_devices,
};
