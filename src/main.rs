use anyhow::{anyhow, Result};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
};
use futures::{sink::SinkExt, stream::StreamExt};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    net::SocketAddr,
    str::FromStr,
    sync::Arc,
    time::Duration,
};
use tokio::sync::Mutex;
use webrtc::{
    api::{
        interceptor_registry::register_default_interceptors,
        media_engine::{
            MediaEngine, MIME_TYPE_G722, MIME_TYPE_OPUS, MIME_TYPE_PCMA, MIME_TYPE_PCMU,
        },
        APIBuilder,
    },
    ice_transport::{
        ice_candidate::{RTCIceCandidate, RTCIceCandidateInit},
        ice_credential_type::RTCIceCredentialType,
        ice_server::RTCIceServer,
    },
    interceptor::registry::Registry,
    peer_connection::{
        configuration::RTCConfiguration, peer_connection_state::RTCPeerConnectionState,
        sdp::session_description::RTCSessionDescription, RTCPeerConnection,
    },
    rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication,
    rtp_transceiver::{
        rtp_codec::{RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType},
        rtp_receiver::RTCRtpReceiver,
    },
    track::{
        track_local::{track_local_static_rtp::TrackLocalStaticRTP, TrackLocal, TrackLocalWriter},
        track_remote::TrackRemote,
    },
};

#[derive(Serialize, Deserialize)]
struct Msg {
    ev: String,
    id: String,
    data: String,
}

fn codec_parameters() -> Vec<RTCRtpCodecParameters> {
    vec![
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: 48000,
                channels: 2,
                sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
                rtcp_feedback: vec![],
            },
            payload_type: 111,
            ..Default::default()
        },
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_G722.to_owned(),
                clock_rate: 8000,
                channels: 0,
                sdp_fmtp_line: "".to_owned(),
                rtcp_feedback: vec![],
            },
            payload_type: 9,
            ..Default::default()
        },
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_PCMU.to_owned(),
                clock_rate: 8000,
                channels: 0,
                sdp_fmtp_line: "".to_owned(),
                rtcp_feedback: vec![],
            },
            payload_type: 0,
            ..Default::default()
        },
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_PCMA.to_owned(),
                clock_rate: 8000,
                channels: 0,
                sdp_fmtp_line: "".to_owned(),
                rtcp_feedback: vec![],
            },
            payload_type: 8,
            ..Default::default()
        },
    ]
}

fn rtc_config() -> RTCConfiguration {
    RTCConfiguration {
        ice_servers: vec![
            RTCIceServer {
                urls: vec![
                    "stun:stun.1.google.com:19302".to_owned(),
                    "stun:stun.2.google.com:19302".to_owned(),
                    "stun:stun.3.google.com:19302".to_owned(),
                    "stun:stun.4.google.com:19302".to_owned(),
                ],
                ..Default::default()
            },
            RTCIceServer {
                urls: vec!["turn:turn.abhisheksarkar.me:3478".to_owned()],
                username: "azureturn".to_owned(),
                credential: "azureturn".to_owned(),
                credential_type: RTCIceCredentialType::Password,
            },
        ],
        ..Default::default()
    }
}

#[derive(Debug)]
struct AppState {
    max_speakers: usize,
    speaker_tracks: HashMap<String, Vec<Arc<TrackLocalStaticRTP>>>,
    speaker_conns: HashMap<String, Arc<RTCPeerConnection>>,
    listeners: HashMap<String, Arc<RTCPeerConnection>>,
    ice_candidates: HashMap<String, VecDeque<String>>,
    tx: Option<tokio::sync::mpsc::Sender<String>>,
    // ice_candidates_state: HashMap<String, bool>,
}

async fn add_track_to_conn(
    id: &str,
    conn: &Arc<RTCPeerConnection>,
    track: &Arc<TrackLocalStaticRTP>,
    tx: tokio::sync::mpsc::Sender<String>,
) -> Result<()> {
    for sender in conn.get_senders().await.iter() {
        if let Some(track1) = sender.track().await {
            if track1.id() == track.id() {
                return Ok(());
            }
        };
    }
    eprintln!("ADDING NEW TRACK TO EXISTING CONN");
    let rtp_sender = conn
        .add_track(Arc::clone(track) as Arc<dyn TrackLocal + Send + Sync>)
        .await?;
    eprintln!("ADDING NEW TRACK TO EXISTING CONN SUCCESS!!");

    // Read incoming RTCP packets
    // Before these packets are returned they are processed by interceptors. For things
    // like NACK this needs to be called.
    tokio::spawn(async move {
        let mut rtcp_buf = vec![0u8; 1500];
        while let Ok((_, _)) = rtp_sender.read(&mut rtcp_buf).await {}
        Result::<()>::Ok(())
    });

    Ok(())
}

async fn handle_negotiation_needed(
    id: String,
    pc: Arc<RTCPeerConnection>,
    tx: tokio::sync::mpsc::Sender<String>,
) {
    if let Ok(offer) = pc.create_offer(None).await {
        if pc.signaling_state()
            != webrtc::peer_connection::signaling_state::RTCSignalingState::Stable
        {
            eprintln!("negotiation needed but state is unstable");
            return;
        }
        if let Ok(_) = pc.set_local_description(offer).await {
            if let Some(offer) = pc.local_description().await {
                if let Ok(offer) = serde_json::to_string(&offer) {
                    let msg = Msg {
                        ev: "offer".to_owned(),
                        id,
                        data: offer,
                    };
                    if let Ok(value) = serde_json::to_string(&msg) {
                        let _ = tx.send(value).await;
                    };
                    println!("negotiation needed. offer set and sent to peer");
                };
            };
        };
    };
}

async fn handle_ice_candidate(
    id: String,
    s: Option<RTCIceCandidate>,
    tx: tokio::sync::mpsc::Sender<String>,
) {
    if let Some(ice_candidate) = s {
        if let Ok(icc) = serde_json::to_string(&ice_candidate) {
            let m = Msg {
                ev: "ice-candidate".to_owned(),
                id: id,
                data: icc,
            };
            if let Ok(m) = serde_json::to_string(&m) {
                let _ = tx.send(m).await;
            }
        };
    };
}

async fn new_peer_conn_with_offer(
    id: String,
    offer: RTCSessionDescription,
    tx: tokio::sync::mpsc::Sender<String>,
    tx_app: tokio::sync::mpsc::Sender<AppStateEv>,
) -> Result<Arc<RTCPeerConnection>> {
    eprintln!("creating new peer conn for: {}", id);

    let mut m = MediaEngine::default();
    for p in codec_parameters() {
        let _ = m.register_codec(p, RTPCodecType::Audio)?;
    }

    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut m)?;

    let api = APIBuilder::new()
        .with_media_engine(m)
        .with_interceptor_registry(registry)
        .build();

    let peer_conn = Arc::new(api.new_peer_connection(rtc_config()).await?);

    eprintln!("sending new peer connection to be inserted");
    let sid = id.clone();
    let (onetx, mut onerx) = tokio::sync::oneshot::channel::<bool>();
    tx_app
        .send(AppStateEv {
            action: "new-speaker".to_owned(),
            data: AppStateEvKind::StrConnTx(sid, Arc::clone(&peer_conn), onetx),
        })
        .await?;
    eprintln!("sent!");
    if let Ok(v) = onerx.try_recv() {
        if !v {
            eprintln!("number of speakers exceeded");
            return Err(anyhow!("num speakers exceeded"));
        }
    }

    eprintln!("Calling set_offer");
    set_offer(id.clone(), &peer_conn, offer, tx.clone()).await?;
    eprintln!("set_offer done");

    let (local_track_chan_tx, mut local_track_chan_rx) =
        tokio::sync::mpsc::channel::<Arc<TrackLocalStaticRTP>>(1);
    let local_track_chan_tx = Arc::new(local_track_chan_tx);

    let sid = id.clone();
    let pc = Arc::downgrade(&peer_conn);
    // eprintln!("setting up on track handler");
    peer_conn
        .on_track(Box::new(
            move |track: Option<Arc<TrackRemote>>, _receiver: Option<Arc<RTCRtpReceiver>>| {
                if let Some(track) = track {
                    // Send PLI periodically
                    // Probably for video
                    let media_ssrc = track.ssrc();
                    let pc2 = pc.clone();
                    tokio::spawn(async move {
                        let mut result = Result::<usize>::Ok(0);
                        while result.is_ok() {
                            let timeout = tokio::time::sleep(std::time::Duration::from_secs(3));
                            tokio::pin!(timeout);

                            tokio::select! {
                                _ = timeout.as_mut() =>{
                                    if let Some(pc) = pc2.upgrade(){
                                        result = pc.write_rtcp(&[Box::new(PictureLossIndication{
                                            sender_ssrc: 0,
                                            media_ssrc,
                                        })]).await.map_err(Into::into);
                                    }else{
                                        break;
                                    }
                                }
                            };
                        }
                    });

                    // Write to local track
                    let local_track_chan_tx2 = Arc::clone(&local_track_chan_tx);
                    let sid = sid.clone();
                    eprintln!("new track from speaker: {}", &sid);
                    tokio::spawn(async move {
                        let rid = uuid::Uuid::new_v4().to_string();
                        let local_track = Arc::new(TrackLocalStaticRTP::new(
                            track.codec().await.capability,
                            format!("{}-{}", &sid, &rid),
                            "webrtc-rs".to_owned(),
                        ));

                        let _ = local_track_chan_tx2.send(Arc::clone(&local_track)).await;
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
    // eprintln!("on track handler ready");

    let sid2 = id.clone();
    let tx1 = tx.clone();
    eprintln!("setting up on ice candidate handler");
    peer_conn
        .on_ice_candidate(Box::new(move |s| {
            let sid2 = sid2.clone();
            let tx1 = tx1.clone();
            tokio::spawn(async move {
                handle_ice_candidate(sid2, s, tx1).await;
            });
            Box::pin(async {})
        }))
        .await;
    eprintln!("on ice candidate handler ready");

    // let pc = peer_conn.clone();
    let pc = Arc::downgrade(&peer_conn);
    let tx1 = tx.clone();
    let sid = id.clone();
    eprintln!("setting up on negotiation needed handler");
    peer_conn
        .on_negotiation_needed(Box::new(move || {
            let pc = pc.clone();
            let tx1 = tx1.clone();
            let id = sid.clone();
            tokio::spawn(async move {
                if let Some(pc) = pc.upgrade() {
                    handle_negotiation_needed(id, pc, tx1).await;
                }
            });
            Box::pin(async {})
        }))
        .await;
    eprintln!("on negotiation needed handler ready");

    // let (connected_tx, _connected_rx) = tokio::sync::mpsc::channel::<bool>(1);
    // let (done_tx, _done_rx) = tokio::sync::mpsc::channel::<bool>(1);
    eprintln!("setting up on peer connection state change handler");
    let tx_app1 = tx_app.clone();
    let sid = id.clone();
    let tx1 = tx.clone();
    let pc = Arc::downgrade(&peer_conn);
    peer_conn
        .on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
            eprintln!("PeerConnection State Changed: {:?}", &s);
            match s {
                webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState::Failed => {
                    // ice restart
                    let id = sid.clone();
                    let tx = tx1.clone();
                    let pc = pc.clone();
                    tokio::spawn(async move{
                        if let Some(pc) = pc.upgrade() {
                            handle_negotiation_needed(id, pc, tx).await;
                        }
                    });
                    
                },
                webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState::Disconnected => {
                    let _ = tx_app1.try_send(AppStateEv{
                        action: "remove-speaker".to_owned(),
                        data: AppStateEvKind::Str(sid.clone()),
                    });
                },
                webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState::Closed => {
                    let _ = tx_app1.try_send(AppStateEv{
                        action: "remove-speaker".to_owned(),
                        data: AppStateEvKind::Str(sid.clone()),
                    });
                }
                _ => {}
            }
            Box::pin(async {})
        }))
        .await;

    eprintln!("waiting to receive track");
    let sid = id.clone();
    if let Some(local_track) = local_track_chan_rx.recv().await {
        eprintln!("received track");
        let _ = tx_app
            .send(AppStateEv {
                action: "add-speaker-track".to_owned(),
                data: AppStateEvKind::StrTrack(sid, local_track),
            })
            .await;
    };

    Ok(peer_conn)
}

async fn new_listener_with_offer(
    id: String,
    offer: RTCSessionDescription,
    tx: tokio::sync::mpsc::Sender<String>,
    tx_app: tokio::sync::mpsc::Sender<AppStateEv>,
) -> Result<Arc<RTCPeerConnection>> {
    eprintln!("creating new listener peer conn for: {}", id);

    let mut m = MediaEngine::default();
    for p in codec_parameters() {
        let _ = m.register_codec(p, RTPCodecType::Audio)?;
    }

    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut m)?;

    let api = APIBuilder::new()
        .with_media_engine(m)
        .with_interceptor_registry(registry)
        .build();

    let peer_conn = Arc::new(api.new_peer_connection(rtc_config()).await?);

    eprintln!("sending new peer connection to be inserted");
    let sid = id.clone();
    tx_app
        .send(AppStateEv {
            action: "new-listener".to_owned(),
            data: AppStateEvKind::StrConn(sid, Arc::clone(&peer_conn)),
        })
        .await?;
    eprintln!("sent!");

    eprintln!("Calling set_offer");
    set_offer(id.clone(), &peer_conn, offer, tx.clone()).await?;
    eprintln!("set_offer done");

    let pc = Arc::downgrade(&peer_conn);
    let tx1 = tx.clone();
    let sid = id.clone();
    eprintln!("[listener] setting up on negotiation needed handler");
    peer_conn
        .on_negotiation_needed(Box::new(move || {
            eprintln!("[onnegotiationstate] -> fired");
            let pc = pc.clone();
            let tx1 = tx1.clone();
            let id = sid.clone();
            tokio::spawn(async move {
                if let Some(pc) = pc.upgrade() {
                    handle_negotiation_needed(id, pc, tx1).await;
                }
            });
            Box::pin(async {})
        }))
        .await;
    eprintln!("[listener] on negotiation needed handler ready");

    let sid2 = id.clone();
    let tx1 = tx.clone();
    eprintln!("[listener] setting up on ice candidate handler");
    peer_conn
        .on_ice_candidate(Box::new(move |s| {
            let sid2 = sid2.clone();
            let tx1 = tx1.clone();
            tokio::spawn(async move {
                handle_ice_candidate(sid2, s, tx1).await;
            });
            Box::pin(async {})
        }))
        .await;
    eprintln!("[listener] on ice candidate handler ready");

    eprintln!("setting up on peer connection state change handler");
    let tx_app1 = tx_app.clone();
    let sid = id.clone();
    let tx1 = tx.clone();
    let pc = Arc::downgrade(&peer_conn);
    peer_conn
        .on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
            eprintln!("Listener PeerConnection State Changed: {:?}", &s);
            match s {
                webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState::Failed => {
                    let id = sid.clone();
                    let tx = tx1.clone();
                    let pc = pc.clone();
                    tokio::spawn(async move{
                        if let Some(pc) = pc.upgrade() {
                            handle_negotiation_needed(id, pc, tx).await;
                        }
                    });
                },
                webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState::Disconnected => {
                    let _ = tx_app1.try_send(AppStateEv{
                        action: "remove-listener".to_owned(),
                        data: AppStateEvKind::Str(sid.clone()),
                    });
                },
                webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState::Closed => {
                    let _ = tx_app1.try_send(AppStateEv{
                        action: "remove-listener".to_owned(),
                        data: AppStateEvKind::Str(sid.clone()),
                    });
                }
                _ => {}
            }
            Box::pin(async {})
        }))
        .await;

    Ok(peer_conn)
}

async fn set_offer(
    id: String,
    conn: &Arc<RTCPeerConnection>,
    offer: RTCSessionDescription,
    tx: tokio::sync::mpsc::Sender<String>,
) -> Result<()> {
    eprintln!("setting remote's offer");
    if conn.signaling_state() != webrtc::peer_connection::signaling_state::RTCSignalingState::Stable
    {
        let mut rollback_sdp = RTCSessionDescription::default();
        rollback_sdp.sdp_type = webrtc::peer_connection::sdp::sdp_type::RTCSdpType::Rollback;
        conn.set_local_description(rollback_sdp).await?;
    }
    conn.set_remote_description(offer).await?;
    let answer = conn.create_answer(None).await?;

    conn.set_local_description(answer).await?;

    let local_desc = match conn.local_description().await {
        Some(lc) => lc,
        None => {
            return Err(anyhow!("local description failed"));
        }
    };

    let m = Msg {
        ev: "answer".to_owned(),
        id,
        data: serde_json::to_string(&local_desc)?,
    };
    let ev_str = serde_json::to_string(&m)?;
    tx.send(ev_str).await?;
    eprintln!("remote's offer set successfully and answer sent");
    Ok(())
}

impl AppState {
    fn new() -> Self {
        AppState {
            max_speakers: 5,
            listeners: HashMap::<String, Arc<RTCPeerConnection>>::new(),
            speaker_tracks: HashMap::<String, Vec<Arc<TrackLocalStaticRTP>>>::new(),
            speaker_conns: HashMap::<String, Arc<RTCPeerConnection>>::new(),
            ice_candidates: HashMap::<String, VecDeque<String>>::new(),
            tx: None,
        }
    }

    async fn distribute_track_speakers(
        &self,
        id: String,
        track: Arc<TrackLocalStaticRTP>,
    ) -> Result<()> {
        println!("Adding track: {}", &id);
        for (id1, conn) in self.speaker_conns.iter() {
            if id.eq(id1) {
                continue;
            }

            if let Some(tx) = &self.tx {
                let tx = tx.clone();
                let _ = add_track_to_conn(&id, conn, &track, tx).await;
            }

            println!("Added track: {} to {}", track.id(), id1);
        }
        Ok(())
    }

    async fn add_track_to_listeners(&self, id: String, track: Arc<TrackLocalStaticRTP>) {
        for (id, conn) in self.listeners.iter() {
            eprintln!("adding track to listener: {}", id);
            if let Some(tx) = &self.tx {
                let tx = tx.clone();
                let _ = add_track_to_conn(&id, conn, &track, tx).await;
            }

            eprintln!("adding track to listener: {} done", id);
        }
    }

    async fn add_tracks_to_listener(&self, id: String) {
        if let Some(tx) = &self.tx {
            if let Some(conn) = self.listeners.get(&id) {
                for (_, tracks) in self.speaker_tracks.iter() {
                    for track in tracks.iter() {
                        eprintln!("adding track to listener: {}", &id);
                        let tx = tx.clone();
                        let _ = add_track_to_conn(&id, conn, track, tx).await;
                        eprintln!("adding track to listener: {} done", &id);
                    }
                }
            }
        }
    }

    async fn distribute_tracks_speaker(&self, id: String) -> Result<()> {
        println!("[distribute_tracks_speaker] for: {}", id.clone());
        if let Some(tx) = &self.tx {
            if let Some(conn) = self.speaker_conns.get(&id) {
                for (id1, tracks) in self.speaker_tracks.iter() {
                    if id1.eq(&id) {
                        continue;
                    }
                    eprintln!(
                        "[distruibute_tracks_speaker] -> handling tracks of: {}",
                        id1
                    );
                    for track in tracks.iter() {
                        eprintln!("Found track: {}", track.id());
                        if track.id().starts_with(&id) {
                            continue;
                        }
                        let tx = tx.clone();
                        let _ = add_track_to_conn(&id, conn, &track, tx).await;
                        println!("added track: {} to {}", track.id(), &id);
                    }
                }
            }
        }
        Ok(())
    }

    async fn add_speaker_track(
        &mut self,
        id: String,
        track: Arc<TrackLocalStaticRTP>,
    ) -> Result<()> {
        println!("adding new speaker track for: {}", &id);
        let track1 = track.clone();
        {
            if let Some(tracks) = self.speaker_tracks.get_mut(&id) {
                tracks.push(track1);
            } else {
                self.speaker_tracks.insert(id.clone(), vec![track1]);
            }
        }

        {
            self.distribute_track_speakers(id.clone(), track.clone())
                .await?;
        }
        {
            self.add_track_to_listeners(id.clone(), track.clone()).await;
        }

        Ok(())
    }

    async fn add_ice_candidates(&mut self, id: &String) -> Result<()> {
        if let Some(conn) = self.speaker_conns.get(id) {
            if let Some(iccs) = self.ice_candidates.get_mut(id) {
                while !iccs.is_empty() {
                    eprintln!("applying ice candidate for: {}", id);
                    if let Some(v) = iccs.pop_front() {
                        if let Ok(candidate) = serde_json::from_str::<RTCIceCandidateInit>(&v) {
                            let _ = conn.add_ice_candidate(candidate).await;
                            eprintln!("applied ice candidate {}", id);
                        }
                    };
                }
            }
        }
        Ok(())
    }
    async fn add_listener_ice_candidates(&mut self, id: &String) -> Result<()> {
        if let Some(conn) = self.listeners.get(id) {
            if let Some(iccs) = self.ice_candidates.get_mut(id) {
                while !iccs.is_empty() {
                    eprintln!("applying ice candidate for: {}", id);
                    if let Some(v) = iccs.pop_front() {
                        if let Ok(candidate) = serde_json::from_str::<RTCIceCandidateInit>(&v) {
                            let _ = conn.add_ice_candidate(candidate).await;
                        }
                    };
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
enum AppStateEvKind {
    Tx(tokio::sync::mpsc::Sender<String>),
    Str(String),
    StrTrack(String, Arc<TrackLocalStaticRTP>),
    StrConn(String, Arc<RTCPeerConnection>),
    StrConnTx(
        String,
        Arc<RTCPeerConnection>,
        tokio::sync::oneshot::Sender<bool>,
    ),
    StrSDP(String, RTCSessionDescription),
    StrSDPTx(
        String,
        RTCSessionDescription,
        tokio::sync::mpsc::Sender<String>,
    ),
    Str2(String, String),
}
#[derive(Debug)]
struct AppStateEv {
    action: String,
    data: AppStateEvKind,
}

struct Context {
    tx: tokio::sync::mpsc::Sender<AppStateEv>,
}

async fn speaker_offer(
    msg: &Msg,
    tx: tokio::sync::mpsc::Sender<String>,
    ctx: Arc<Context>,
) -> Result<()> {
    let id = msg.id.clone();
    let offer = serde_json::from_str::<RTCSessionDescription>(&msg.data)?;
    ctx.tx
        .send(AppStateEv {
            action: "speaker-offer".to_owned(),
            data: AppStateEvKind::StrSDPTx(id, offer, tx.clone()),
        })
        .await?;
    Ok(())
}

async fn speaker_answer(msg: &Msg, ctx: Arc<Context>) -> Result<()> {
    let id = msg.id.clone();
    let answer = serde_json::from_str::<RTCSessionDescription>(&msg.data)?;
    ctx.tx
        .send(AppStateEv {
            action: "speaker-answer".to_owned(),
            data: AppStateEvKind::StrSDP(id, answer),
        })
        .await?;
    Ok(())
}

async fn listener_offer(
    msg: &Msg,
    tx: tokio::sync::mpsc::Sender<String>,
    ctx: Arc<Context>,
) -> Result<()> {
    let id = msg.id.clone();
    let offer = serde_json::from_str::<RTCSessionDescription>(&msg.data)?;
    ctx.tx
        .send(AppStateEv {
            action: "listener-offer".to_owned(),
            data: AppStateEvKind::StrSDPTx(id, offer, tx.clone()),
        })
        .await?;
    Ok(())
}

async fn listener_answer(msg: &Msg, ctx: Arc<Context>) -> Result<()> {
    let id = msg.id.clone();
    let answer = serde_json::from_str::<RTCSessionDescription>(&msg.data)?;
    ctx.tx
        .send(AppStateEv {
            action: "listener-answer".to_owned(),
            data: AppStateEvKind::StrSDP(id, answer),
        })
        .await?;
    Ok(())
}

async fn new_ice_candidate(data: &Msg, ctx: Arc<Context>) -> Result<()> {
    // let ice_candidate = serde_json::from_str::<RTCIceCandidateInit>(&data.data)?;
    ctx.tx
        .send(AppStateEv {
            action: "ice-candidate".to_owned(),
            data: AppStateEvKind::Str2(data.id.clone(), data.data.clone()),
        })
        .await?;
    Ok(())
}

async fn websocket(stream: WebSocket, state: Arc<Context>) {
    let (mut send, mut recv) = stream.split();

    let (tx, mut rx) = tokio::sync::mpsc::channel(100);

    let tx_app = state.tx.clone();
    let _ = tx_app
        .send(AppStateEv {
            action: "signal-sender".to_owned(),
            data: AppStateEvKind::Tx(tx.clone()),
        })
        .await;

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
                eprintln!("ws received for {} as {}", &m.id, &m.ev);
                match m.ev.as_str() {
                    "speaker-offer" => match speaker_offer(&m, tx1.clone(), state.clone()).await {
                        Ok(_) => {}
                        Err(e) => {
                            eprintln!("ws:speaker-offer failed: {:?}", e);
                        }
                    },
                    "speaker-answer" => match speaker_answer(&m, state.clone()).await {
                        Ok(_) => {}
                        Err(e) => {
                            eprintln!("ws:speaker-answer failed: {:?}", e);
                        }
                    },
                    "listener-offer" => {
                        match listener_offer(&m, tx1.clone(), state.clone()).await {
                            Ok(_) => {}
                            Err(e) => {
                                eprintln!("listenerPeerConn failed: {:?}", e);
                            }
                        }
                    }
                    "listener-answer" => match listener_answer(&m, state.clone()).await {
                        Ok(_) => {}
                        Err(e) => {
                            eprintln!("ws: listener-answer failed: {}", e);
                        }
                    },
                    "ice-candidate" => {
                        eprintln!("new ice candidate");
                        match new_ice_candidate(&m, state.clone()).await {
                            Ok(_) => {}
                            Err(e) => {
                                eprintln!("new ice-candidate failed: {}", e);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    });

    tokio::select! {
        _ = (&mut send_task) => recv_task.abort(),
        _ = (&mut recv_task) => send_task.abort(),
    };
}

async fn ws_handler(ws: WebSocketUpgrade, State(ctx): State<Arc<Context>>) -> impl IntoResponse {
    ws.on_upgrade(|socket| websocket(socket, ctx))
}

#[tokio::main]
async fn main() -> Result<()> {
    let app_state = Arc::new(Mutex::new(AppState::new()));
    let (tx, mut rx) = tokio::sync::mpsc::channel::<AppStateEv>(1);
    let ctx = Context { tx: tx.clone() };
    let app = axum::Router::with_state(Arc::new(ctx)).route("/ws", axum::routing::get(ws_handler));

    tokio::spawn(async move {
        while let Some(data) = rx.recv().await {
            match data.action.as_str() {
                "new-speaker" => {
                    if let AppStateEvKind::StrConnTx(id, conn, tx) = data.data {
                        {
                            let id = id.clone();
                            let mut app_state = app_state.lock().await;
                            if app_state.speaker_conns.len() >= 5 {
                                let _ = tx.send(false);
                                return;
                            }
                            app_state.speaker_conns.insert(id.clone(), conn);
                            let _ = tx.send(true);
                            let _ = app_state.distribute_tracks_speaker(id).await;
                        }

                        {
                            let app_state = app_state.clone();
                            let id = id.clone();
                            tokio::spawn(async move {
                                let app_state = app_state.clone();
                                let mut interval =
                                    tokio::time::interval(Duration::from_millis(100));
                                let id = id.clone();
                                loop {
                                    interval.tick().await;
                                    // let _ = intx2.send(true).await;
                                    let mut app_state = app_state.lock().await;
                                    let _ = app_state.add_ice_candidates(&id).await;
                                }
                            });
                        }
                    }
                }
                "speaker-offer" => {
                    if let AppStateEvKind::StrSDPTx(id, offer, tx_ws) = data.data {
                        let app_state = app_state.lock().await;
                        if let Some(conn) = app_state.speaker_conns.get(&id) {
                            let _ = set_offer(id.clone(), conn, offer, tx_ws.clone()).await;
                        } else {
                            let id = id.clone();
                            let tx_ws = tx_ws.clone();
                            let tx = tx.clone();
                            tokio::spawn(async move {
                                let _ = new_peer_conn_with_offer(id, offer, tx_ws, tx).await;
                            });
                        };
                    };
                }
                "speaker-answer" => {
                    if let AppStateEvKind::StrSDP(id, answer) = data.data {
                        let app_state = app_state.lock().await;
                        if let Some(conn) = app_state.speaker_conns.get(&id) {
                            if conn.signaling_state() != webrtc::peer_connection::signaling_state::RTCSignalingState::Stable {
                                    let _ = conn.set_remote_description(answer).await;
                                } else {
                                    eprintln!("signalling state is stable. cannot set answer");
                                }
                        }
                    }
                }
                "add-speaker-track" => {
                    if let AppStateEvKind::StrTrack(id, track) = data.data {
                        let mut app_state = app_state.lock().await;
                        let _ = app_state.add_speaker_track(id, track).await;
                    }
                }
                "ice-candidate" => {
                    if let AppStateEvKind::Str2(id, ice_candidate) = data.data {
                        let mut app_state = app_state.lock().await;
                        if !app_state.ice_candidates.contains_key(&id) {
                            app_state
                                .ice_candidates
                                .insert(id.clone(), VecDeque::<String>::new());
                        }
                        if let Some(v) = app_state.ice_candidates.get_mut(&id) {
                            v.push_back(ice_candidate);
                        }
                    }
                }
                "remove-speaker" => {
                    if let AppStateEvKind::Str(id) = data.data {
                        eprint!("remove speaker fired for: {}", &id);
                        let mut app_state = app_state.lock().await;
                        // Remove tracks from speakers
                        for (sid, conn) in app_state.speaker_conns.iter() {
                            if sid.eq(&id) {
                                let _ = conn.close().await;
                            } else {
                                for sender in conn.get_senders().await.iter() {
                                    if let Some(track) = sender.track().await {
                                        if track.id().starts_with(&id) {
                                            let _ = conn.remove_track(sender).await;
                                        }
                                    };
                                }
                            }
                        }
                        // From listeners
                        for (_, conn) in app_state.listeners.iter() {
                            for sender in conn.get_senders().await.iter() {
                                if let Some(track) = sender.track().await {
                                    if track.id().starts_with(&id) {
                                        let _ = conn.remove_track(sender).await;
                                    }
                                };
                            }
                        }
                        // Remove speakers from entry
                        if let Some(_) = app_state.speaker_conns.get(&id) {
                            app_state.speaker_conns.remove(&id);
                        }

                        if let Some(_) = app_state.speaker_tracks.get(&id) {
                            app_state.speaker_tracks.remove(&id);
                        }
                    };
                }
                "listener-offer" => {
                    if let AppStateEvKind::StrSDPTx(id, offer, tx_ws) = data.data {
                        let app_state = app_state.lock().await;
                        if let Some(conn) = app_state.listeners.get(&id) {
                            let _ = set_offer(id.clone(), conn, offer, tx_ws.clone()).await;
                        } else {
                            let id = id.clone();
                            let tx_ws = tx_ws.clone();
                            let tx = tx.clone();
                            tokio::spawn(async move {
                                let _ = new_listener_with_offer(id, offer, tx_ws, tx).await;
                            });
                        };
                    };
                }
                "new-listener" => {
                    if let AppStateEvKind::StrConn(id, conn) = data.data {
                        {
                            let id = id.clone();
                            let mut app_state = app_state.lock().await;
                            app_state.listeners.insert(id.clone(), conn);
                            eprintln!("new-listener success");
                            app_state.add_tracks_to_listener(id).await;
                        }
                        {
                            let app_state = app_state.clone();
                            let id = id.clone();
                            tokio::spawn(async move {
                                let app_state = app_state.clone();
                                let mut interval =
                                    tokio::time::interval(Duration::from_millis(100));
                                let id = id.clone();
                                loop {
                                    interval.tick().await;
                                    // let _ = intx2.send(true).await;
                                    let mut app_state = app_state.lock().await;
                                    let _ = app_state.add_listener_ice_candidates(&id).await;
                                }
                            });
                            eprintln!("Applying ice-candidates for listener");
                        }
                    }
                }
                "remove-listener" => {
                    if let AppStateEvKind::Str(id) = data.data {
                        let mut app_state = app_state.lock().await;
                        if let Some(conn) = app_state.listeners.get(&id) {
                            let _ = conn.close().await;
                            app_state.listeners.remove(&id);
                        }
                    }
                }
                "listener-answer" => {
                    if let AppStateEvKind::StrSDP(id, answer) = data.data {
                        let app_state = app_state.lock().await;
                        if let Some(conn) = app_state.listeners.get(&id) {
                            if conn.signaling_state() != webrtc::peer_connection::signaling_state::RTCSignalingState::Stable {
                                    let _ = conn.set_remote_description(answer).await;
                                } else {
                                    eprintln!("signalling state is stable. cannot set answer");
                                }
                        }
                    }
                }
                "signal-sender" => {
                    if let AppStateEvKind::Tx(tx) = data.data {
                        let mut app_state = app_state.lock().await;
                        app_state.tx = Some(tx);
                    }
                }
                _ => {}
            }
        }
    });

    let matches = clap::builder::Command::new("radio")
        .author("Abhishek Sarkar<rakrashba@outlook.com>")
        .version("0.0.1")
        .about("A Hackathon project - my first year @ Microsoft. 2022")
        .arg(
            clap::builder::Arg::new("prod")
                .short('p')
                .long("prod")
                .takes_value(false)
                .help("decides whether to run in prod or local mode"),
        )
        .arg(
            clap::builder::Arg::new("addr")
                .short('a')
                .long("addr")
                .takes_value(true)
                .value_name("ADDR")
                .help("address to run this app on"),
        )
        .get_matches();

    let mut socket_addr = SocketAddr::from_str(&"0.0.0.0:29874").unwrap();
    if let Some(addr) = matches.get_one::<String>("addr") {
        socket_addr = SocketAddr::from_str(addr).unwrap();
    }
    let mut tls_config: Option<axum_server::tls_rustls::RustlsConfig> = None;
    if matches.contains_id("prod") {
        // /etc/letsencrypt/live/msh22.abhisheksarkar.me/fullchain.pem
        if let Ok(cert_path) = std::env::var("CERT_PATH") {
            // /etc/letsencrypt/live/msh22.abhisheksarkar.me/privkey.pem
            if let Ok(key_path) = std::env::var("KEY_PATH") {
                if let Ok(config) = axum_server::tls_rustls::RustlsConfig::from_pem_file(
                    std::path::PathBuf::from(cert_path),
                    std::path::PathBuf::from(key_path),
                )
                .await
                {
                    tls_config = Some(config);
                } else {
                    eprintln!("error trying to open tls files");
                }
            } else {
                eprintln!("KEY_PATH not set");
            };
        } else {
            eprintln!("CERT_PATH not set");
        };
    }

    if let Some(tls_config) = tls_config {
        eprintln!("running https @: {:?}", &socket_addr);
        axum_server::bind_rustls(socket_addr, tls_config)
            .serve(app.into_make_service())
            .await?;
    } else {
        eprintln!("running at {:?}", &socket_addr);
        axum::Server::bind(&socket_addr)
            .serve(app.into_make_service())
            .await?;
    }

    Ok(())
}
