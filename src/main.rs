use anyhow::{anyhow, Result};
use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::IntoResponse,
};
use futures::{sink::SinkExt, stream::StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::to_string;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::signal;
use webrtc::{
    api::{
        interceptor_registry::register_default_interceptors,
        media_engine::{MediaEngine, MIME_TYPE_OPUS},
        APIBuilder,
    },
    ice_transport::{ice_credential_type::RTCIceCredentialType, ice_server::RTCIceServer},
    interceptor::registry::Registry,
    peer_connection::{
        configuration::RTCConfiguration, peer_connection_state::RTCPeerConnectionState,
        sdp::session_description::RTCSessionDescription,
    },
    rtp_transceiver::{
        rtp_codec::{RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType},
        rtp_receiver::RTCRtpReceiver,
    },
    track::{
        track_local::{track_local_static_rtp::TrackLocalStaticRTP, TrackLocalWriter},
        track_remote::TrackRemote,
    },
};

#[derive(Serialize, Deserialize)]
struct Msg {
    ev: String,
    data: String,
}

fn codec_capability() -> RTCRtpCodecCapability {
    RTCRtpCodecCapability {
        mime_type: MIME_TYPE_OPUS.to_owned(),
        clock_rate: 48000,
        channels: 2,
        sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
        rtcp_feedback: vec![],
    }
}
fn codec_parameters() -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        capability: codec_capability(),
        payload_type: 111,
        ..Default::default()
    }
}

async fn speakerPeerConn(data: &String, mut tx: tokio::sync::mpsc::Sender<String>) -> Result<()> {
    let offer = serde_json::from_str::<RTCSessionDescription>(data)?;

    let mut m = MediaEngine::default();
    m.register_codec(codec_parameters(), RTPCodecType::Audio);

    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut m)?;

    let api = APIBuilder::new()
        .with_media_engine(m)
        .with_interceptor_registry(registry)
        .build();
    let config = RTCConfiguration {
        ice_servers: vec![
            RTCIceServer {
                urls: vec!["stun:stun.1.google.com:19302".to_owned()],
                ..Default::default()
            },
            RTCIceServer {
                urls: vec!["turn:localhost:3478".to_owned()],
                username: "user".to_owned(),
                credential: "pwd".to_owned(),
                credential_type: RTCIceCredentialType::Password,
            },
        ],
        ..Default::default()
    };

    let peer_conn = Arc::new(api.new_peer_connection(config).await?);
    peer_conn
        .add_transceiver_from_kind(RTPCodecType::Audio, &[])
        .await?;

    let (local_track_tx, local_track_rx) =
        tokio::sync::mpsc::channel::<Arc<TrackLocalStaticRTP>>(1);

    let local_track_tx = Arc::new(local_track_tx);

    let pc = Arc::downgrade(&peer_conn);

    peer_conn
        .on_track(Box::new(
            move |track: Option<Arc<TrackRemote>>, _receiver: Option<Arc<RTCRtpReceiver>>| {
                if let Some(track) = track {
                    let local_track_tx2 = Arc::clone(&local_track_tx);
                    tokio::spawn(async move {
                        let local_track = Arc::new(TrackLocalStaticRTP::new(
                            track.codec().await.capability,
                            "audio".to_owned(),
                            "webrtc-rs".to_owned(),
                        ));
                        let _ = local_track_tx2.send(Arc::clone(&local_track)).await;

                        while let Ok((rtp, _)) = track.read_rtp().await {
                            if let Err(e) = local_track.write_rtp(&rtp).await {
                                if webrtc::Error::ErrClosedPipe != e {
                                    eprintln!("output track has error: {}. breaking!", e);
                                    break;
                                } else {
                                    eprintln!("output track got error: {}", e);
                                }
                            }
                        }
                    });
                }
                Box::pin(async {})
            },
        ))
        .await;

    peer_conn
        .on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
            // TODO:
            Box::pin(async {})
        }))
        .await;

    // Set Offer
    peer_conn.set_remote_description(offer).await?;

    // Gen Answer
    let answer = peer_conn.create_answer(None).await?;

    let mut gather_complete = peer_conn.gathering_complete_promise().await;

    peer_conn.set_local_description(answer).await?;

    let _ = gather_complete.recv().await;

    let local_desc = match peer_conn.local_description().await {
        Some(lc) => lc,
        None => {
            return Err(anyhow!("local description failed"));
        }
    };

    let b64 = base64::encode(serde_json::to_string(&local_desc)?);
    let m = Msg {
        ev: "answer".to_owned(),
        data: b64,
    };
    let ev_str = serde_json::to_string(&m)?;
    tx.send(ev_str).await?;

    Ok(())
}

async fn listenerPeerConn(data: &String, mut tx: tokio::sync::mpsc::Sender<String>) -> Result<()> {
    Ok(())
}

async fn websocket(stream: WebSocket) {
    let (mut send, mut recv) = stream.split();

    let (mut tx, mut rx) = tokio::sync::mpsc::channel(100);

    let mut send_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if send.send(Message::Text(msg)).await.is_err() {
                break;
            }
        }
    });

    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(Message::Text(msg))) = recv.next().await {
            let tx1 = tx.clone();
            if let Ok(m) = serde_json::from_str::<Msg>(msg.as_str()) {
                match m.ev.as_str() {
                    "speakerOffer" => match speakerPeerConn(&m.data, tx1.clone()).await {
                        Ok(_) => {}
                        Err(e) => {
                            eprintln!("speakerPeerConn failed: {:?}", e);
                        }
                    },
                    "listenerOffer" => match listenerPeerConn(&m.data, tx1.clone()).await {
                        Ok(_) => {}
                        Err(e) => {
                            eprintln!("listenerPeerConn failed: {:?}", e);
                        }
                    },
                    _ => {}
                }
            }
        }
    });
}

async fn ws_handler(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(|socket| websocket(socket))
}

#[tokio::main]
async fn main() -> Result<()> {
    let app = axum::Router::new()
        .route("/", axum::routing::get(|| async { "Hello World" }))
        .route("/ws", axum::routing::get(ws_handler));

    axum::Server::bind(&"0.0.0.0:3000".parse().unwrap())
        .serve(app.into_make_service())
        .await?;

    Ok(())
}
