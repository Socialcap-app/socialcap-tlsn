pub mod axum_websocket;
pub mod tcp;
pub mod websocket;

use async_trait::async_trait;
use axum::{
    extract::{rejection::JsonRejection, FromRequestParts, Query, State},
    http::{header, request::Parts, StatusCode},
    response::{IntoResponse, Json, Response},
};
use axum_macros::debug_handler;
use base64::Engine;
use chrono::Utc;
use mina_signer::{
    keypair::Keypair, NetworkId, SecKey, Signature as MinaSignature, Signer as MinaSigner,
};
use sha1::digest::generic_array::GenericArray;
use tlsn_core::signature::{Data, MinaSchnorrSignature, TLSNSignature};
// use p256::ecdsa::{Signature, TLSNSigningKey};
use tlsn_verifier::tls::{Verifier, VerifierConfig};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::compat::TokioAsyncReadCompatExt;
use tracing::{debug, error, info, trace};
use uuid::Uuid;

use p256::pkcs8::DecodePrivateKey;

use crate::{
    domain::notary::{
        NotarizationRequestQuery, NotarizationSessionRequest, NotarizationSessionResponse,
        NotaryGlobals, SessionData,
    },
    error::NotaryServerError,
    service::{
        axum_websocket::{header_eq, WebSocketUpgrade},
        tcp::{tcp_notarize, TcpUpgrade},
        websocket::websocket_notarize,
    },
};
use signature::{Error, Signer};

#[derive(Clone, Debug)]
pub enum TLSNSigningKey {
    MinaSchnorr(SecKey),
    P256(p256::ecdsa::SigningKey),
}

impl From<mina_signer::seckey::SecKey> for TLSNSigningKey {
    fn from(key: SecKey) -> Self {
        Self::MinaSchnorr(key)
    }
}

impl From<p256::ecdsa::SigningKey> for TLSNSigningKey {
    fn from(key: p256::ecdsa::SigningKey) -> Self {
        Self::P256(key)
    }
}

impl TLSNSigningKey {
    pub fn read_default_schnorr_pem_file() -> Self {
        Self::MinaSchnorr(SecKey::from_bytes(&[0u8; 32]).unwrap())
    }

    pub fn read_schnorr_pem_file(path: &str) -> Result<Self, ()> {
        Ok(Self::MinaSchnorr(
            SecKey::from_base58("EKFSmntAEAPm5CnYMsVpfSEuyNfbXfxy2vHW8HPxGyPPgm5xyRtN").unwrap(),
        ))
        // println!("path: {:?}", path);
        // Ok(Self::MinaSchnorr(SecKey::from_base58(path).unwrap()))
    }

    pub fn read_p256_pem_file(path: &str) -> Result<Self, eyre::Error> {
        let signing_key = p256::ecdsa::SigningKey::read_pkcs8_pem_file(path)
            .map_err(|err| eyre::eyre!("Failed to parse P256 PEM file: {}", err))?;

        Ok(Self::P256(signing_key))

        // let signing_key_str = std::fs::read_to_string(DEFAULT_PEM_PATH)
        // .map_err(|_| ())?;

        // Ok(Self::P256(p256::ecdsa::SigningKey::read_pkcs8_pem_file(signing_key_str).unwrap()))

        // Ok(Self::P256(p256::ecdsa::SigningKey::read_pkcs8_pem_file(path).unwrap()))
    }
}

/// Sign the provided message bytestring using `Self` (e.g. a cryptographic key
/// or connection to an HSM), returning a digital signature.
impl Signer<tlsn_core::TLSNSignature> for TLSNSigningKey {
    fn sign(&self, msg: &[u8]) -> tlsn_core::TLSNSignature {
        self.try_sign(msg).expect("signature operation failed")
    }

    fn try_sign(&self, msg: &[u8]) -> Result<tlsn_core::TLSNSignature, signature::Error> {
        match self {
            TLSNSigningKey::MinaSchnorr(sk) => {
                let mut ctx =
                    mina_signer::create_kimchi::<tlsn_core::signature::Data>(NetworkId::TESTNET);
                let key_pair =
                    Keypair::from_secret_key(sk.clone()).map_err(|_| signature::Error::new())?;
                let sig = ctx.sign(&key_pair, &Data::from(msg));
                Ok(TLSNSignature::MinaSchnorr(MinaSchnorrSignature(sig)))
            }
            TLSNSigningKey::P256(sk) => {
                let sig = sk.try_sign(msg)?;
                Ok(tlsn_core::TLSNSignature::P256(sig))
            }
        }
    }
}

/// A wrapper enum to facilitate extracting TCP connection for either WebSocket or TCP clients,
/// so that we can use a single endpoint and handler for notarization for both types of clients
pub enum ProtocolUpgrade {
    Tcp(TcpUpgrade),
    Ws(WebSocketUpgrade),
}

#[async_trait]
impl<S> FromRequestParts<S> for ProtocolUpgrade
where
    S: Send + Sync,
{
    type Rejection = NotaryServerError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        // Extract tcp connection for websocket client
        if header_eq(&parts.headers, header::UPGRADE, "websocket") {
            let extractor = WebSocketUpgrade::from_request_parts(parts, state)
                .await
                .map_err(|err| NotaryServerError::BadProverRequest(err.to_string()))?;
            return Ok(Self::Ws(extractor));
        // Extract tcp connection for tcp client
        } else if header_eq(&parts.headers, header::UPGRADE, "tcp") {
            let extractor = TcpUpgrade::from_request_parts(parts, state)
                .await
                .map_err(|err| NotaryServerError::BadProverRequest(err.to_string()))?;
            return Ok(Self::Tcp(extractor));
        } else {
            return Err(NotaryServerError::BadProverRequest(
                "Upgrade header is not set for client".to_string(),
            ));
        }
    }
}

/// Handler to upgrade protocol from http to either websocket or underlying tcp depending on the type of client
/// the session_id parameter is also extracted here to fetch the configuration parameters
/// that have been submitted in the previous request to /session made by the same client
pub async fn upgrade_protocol(
    protocol_upgrade: ProtocolUpgrade,
    State(notary_globals): State<NotaryGlobals>,
    Query(params): Query<NotarizationRequestQuery>,
) -> Response {
    info!("Received upgrade protocol request");
    let session_id = params.session_id;
    // Fetch the configuration data from the store using the session_id
    // This also removes the configuration data from the store as each session_id can only be used once
    let max_transcript_size = match notary_globals.store.lock().await.remove(&session_id) {
        Some(data) => data.max_transcript_size,
        None => {
            let err_msg = format!("Session id {} does not exist", session_id);
            error!(err_msg);
            return NotaryServerError::BadProverRequest(err_msg).into_response();
        }
    };
    // This completes the HTTP Upgrade request and returns a successful response to the client, meanwhile initiating the websocket or tcp connection
    match protocol_upgrade {
        ProtocolUpgrade::Ws(ws) => ws.on_upgrade(move |socket| {
            websocket_notarize(socket, notary_globals, session_id, max_transcript_size)
        }),
        ProtocolUpgrade::Tcp(tcp) => tcp.on_upgrade(move |stream| {
            tcp_notarize(stream, notary_globals, session_id, max_transcript_size)
        }),
    }
}

/// Handler to initialize and configure notarization for both TCP and WebSocket clients
#[debug_handler(state = NotaryGlobals)]
pub async fn initialize(
    State(notary_globals): State<NotaryGlobals>,
    payload: Result<Json<NotarizationSessionRequest>, JsonRejection>,
) -> impl IntoResponse {
    info!(
        ?payload,
        "Received request for initializing a notarization session"
    );

    // Parse the body payload
    let payload = match payload {
        Ok(payload) => payload,
        Err(err) => {
            error!("Malformed payload submitted for initializing notarization: {err}");
            return NotaryServerError::BadProverRequest(err.to_string()).into_response();
        }
    };

    // Ensure that the max_transcript_size submitted is not larger than the global max limit configured in notary server
    if payload.max_transcript_size > Some(notary_globals.notarization_config.max_transcript_size) {
        error!(
            "Max transcript size requested {:?} exceeds the maximum threshold {:?}",
            payload.max_transcript_size, notary_globals.notarization_config.max_transcript_size
        );
        return NotaryServerError::BadProverRequest(
            "Max transcript size requested exceeds the maximum threshold".to_string(),
        )
        .into_response();
    }

    let prover_session_id = Uuid::new_v4().to_string();

    // Store the configuration data in a temporary store
    notary_globals.store.lock().await.insert(
        prover_session_id.clone(),
        SessionData {
            max_transcript_size: payload.max_transcript_size,
            created_at: Utc::now(),
        },
    );

    trace!("Latest store state: {:?}", notary_globals.store);

    // Return the session id in the response to the client
    (
        StatusCode::OK,
        Json(NotarizationSessionResponse {
            session_id: prover_session_id,
        }),
    )
        .into_response()
}

/// Run the notarization
pub async fn notary_service<T: AsyncWrite + AsyncRead + Send + Unpin + 'static>(
    socket: T,
    signing_key: &TLSNSigningKey,
    session_id: &str,
    max_transcript_size: Option<usize>,
) -> Result<(), NotaryServerError> {
    debug!(?session_id, "Starting notarization...");

    let mut config_builder = VerifierConfig::builder();

    config_builder = config_builder.id(session_id);

    if let Some(max_transcript_size) = max_transcript_size {
        config_builder = config_builder.max_transcript_size(max_transcript_size);
    }

    let config = config_builder.build()?;

    Verifier::new(config)
        .notarize::<_, TLSNSignature>(socket.compat(), signing_key)
        .await?;

    // match signing_key {
    //     TLSNSigningKey::MinaSchnorr(key) => {
    //         Verifier::new(config)
    //             .notarize::<_, TLSNSignature>(socket.compat(), signing_key)
    //             .await?;
    //     }
    //     TLSNSigningKey::P256(key) => {
    //         Verifier::new(config)
    //             .notarize::<_, p256::ecdsa::Signature>(socket.compat(), key)
    //             .await?;
    //     },
    //     // _ => unimplemented!("Mina Schnorr notarization is not yet implemented")
    // }

    Ok(())
}
