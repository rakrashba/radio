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
use std::{collections::HashMap, default, sync::Arc};
use tokio::sync::Mutex;
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

fn rtc_config() -> RTCConfiguration {
    RTCConfiguration {
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
    }
}

struct Speaker {
    id: String,
    track: Option<Arc<TrackLocalStaticRTP>>, // Speakers produce in this
    send_conn: Option<Arc<RTCPeerConnection>>, // Speakers consume this
}

impl Default for Speaker {
    fn default() -> Self {
        Speaker {
            id: "".to_owned(),
            track: None,
            send_conn: None,
        }
    }
}

impl Speaker {
    fn add_track(&mut self, track: Arc<TrackLocalStaticRTP>) {
        self.track = Some(track.clone().to_owned());
    }

    fn add_send_conn(&mut self, conn: Arc<RTCPeerConnection>) {
        self.send_conn = Some(conn.clone().to_owned());
    }
}

struct Listener {
    id: String,
    conn: Option<Arc<RTCPeerConnection>>,
}

impl Default for Listener {
    fn default() -> Self {
        Listener {
            id: "".to_owned(),
            conn: None,
        }
    }
}

impl Listener {
    fn add_conn(&mut self, conn: Arc<RTCPeerConnection>) {
        self.conn = Some(conn.clone().to_owned());
    }
}

struct AppState {
    max_speakers: usize,
    speakers: HashMap<String, Arc<Mutex<Speaker>>>,
    listeners: Vec<Arc<Listener>>,
}

impl AppState {
    fn new() -> Self {
        AppState {
            max_speakers: 5,
            speakers: HashMap::<String, Arc<Mutex<Speaker>>>::new(),
            listeners: vec![],
        }
    }

    fn new_speaker(&mut self, id: String) -> Result<()> {
        if self.speakers.len() >= self.max_speakers {
            return Err(anyhow!("max speakers exceeded"));
        }
        let mut s = Speaker::default();
        s.id = id;
        self.speakers.insert(s.id.clone(), Arc::new(Mutex::new(s)));
        Ok(())
    }

    async fn add_speaker_track(
        &mut self,
        id: String,
        track: Arc<TrackLocalStaticRTP>,
    ) -> Result<()> {
        let mut speaker = self.speakers[id.as_str()].lock().await;
        speaker.add_track(track.clone());
        let track1 = track.clone();
        for l in self.listeners.iter() {
            if let Some(conn) = &l.conn {
                let rtp_sender = conn
                    .add_track(Arc::clone(&track1) as Arc<dyn TrackLocal + Send + Sync>)
                    .await?;
                tokio::spawn(async move {
                    let mut rtcp_buf = vec![0u8; 1500];
                    while let Ok((_, _)) = rtp_sender.read(&mut rtcp_buf).await {}
                    Result::<()>::Ok(())
                });
            }
        }
        for (sid, speaker) in self.speakers.iter() {
            if sid.eq(&id) {
                continue;
            }
            let s = speaker.lock().await;
            if let Some(conn) = &s.send_conn {
                let mut flag = false;
                for sender in conn.get_senders().await.iter() {
                    if let Some(sender_track) = sender.track().await {
                        if sid.eq(sender_track.id()) {
                            flag = true;
                            break;
                        }
                    }
                }
                if flag {
                    continue;
                }

                let rtp_sender = conn
                    .add_track(Arc::clone(&track1) as Arc<dyn TrackLocal + Send + Sync>)
                    .await?;
                tokio::spawn(async move {
                    let mut rtcp_buf = vec![0u8; 1500];
                    while let Ok((_, _)) = rtp_sender.read(&mut rtcp_buf).await {}
                    Result::<()>::Ok(())
                });
            }
        }

        Ok(())
    }

    async fn add_speaker_conn(&mut self, id: String, conn: Arc<RTCPeerConnection>) -> Result<()> {
        for (sid, speaker) in self.speakers.iter() {
            if sid.eq(&id) {
                continue;
            }
            if let Some(track) = &speaker.lock().await.track {
                let rtp_sender = conn
                    .add_track(Arc::clone(&track) as Arc<dyn TrackLocal + Send + Sync>)
                    .await?;
                tokio::spawn(async move {
                    let mut rtcp_buf = vec![0u8; 1500];
                    while let Ok((_, _)) = rtp_sender.read(&mut rtcp_buf).await {}
                    Result::<()>::Ok(())
                });
            }
        }
        let mut speaker = self.speakers[id.as_str()].lock().await;
        speaker.add_send_conn(conn);
        Ok(())
    }

    async fn new_listener(&mut self, id: String, conn: Arc<RTCPeerConnection>) -> Result<()> {
        for (_id, speaker) in self.speakers.iter() {
            if let Some(track) = &speaker.lock().await.track {
                let rtp_sender = conn
                    .add_track(Arc::clone(&track) as Arc<dyn TrackLocal + Send + Sync>)
                    .await?;
                tokio::spawn(async move {
                    let mut rtcp_buf = vec![0u8; 1500];
                    while let Ok((_, _)) = rtp_sender.read(&mut rtcp_buf).await {}
                    Result::<()>::Ok(())
                });
            }
        }
        let mut l = Listener::default();
        l.id = id;
        l.add_conn(conn);
        self.listeners.push(Arc::new(l));
        Ok(())
    }
}

enum AppStateEvKind {
    Str(String),
    StrTrack(String, Arc<TrackLocalStaticRTP>),
    StrConn(String, Arc<RTCPeerConnection>),
    Conn(Arc<RTCPeerConnection>),
}
struct AppStateEv {
    action: String,
    data: AppStateEvKind,
}

struct Context {
    tx: tokio::sync::mpsc::Sender<AppStateEv>,
}

#[derive(Deserialize)]
struct WSD {
    sdp: RTCSessionDescription,
    id: String,
}

async fn speakerRecvPeerConn(
    data: &str,
    tx: tokio::sync::mpsc::Sender<String>,
    ctx: Arc<Context>,
) -> Result<()> {
    let data = serde_json::from_str::<WSD>(data)?;
    let offer = data.sdp;

    let mut m = MediaEngine::default();
    m.register_codec(codec_parameters(), RTPCodecType::Audio)?;

    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut m)?;

    let api = APIBuilder::new()
        .with_media_engine(m)
        .with_interceptor_registry(registry)
        .build();

    let peer_conn = Arc::new(api.new_peer_connection(rtc_config()).await?);
    peer_conn
        .add_transceiver_from_kind(RTPCodecType::Audio, &[])
        .await?;

    let (local_track_chan_tx, mut local_track_chan_rx) =
        tokio::sync::mpsc::channel::<Arc<TrackLocalStaticRTP>>(1);
    let local_track_chan_tx = Arc::new(local_track_chan_tx);

    let sid = data.id.clone();
    let pc = Arc::downgrade(&peer_conn);
    peer_conn
        .on_track(Box::new(
            move |track: Option<Arc<TrackRemote>>, _receiver: Option<Arc<RTCRtpReceiver>>| {
                if let Some(track) = track {
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

                    let local_track_chan_tx2 = Arc::clone(&local_track_chan_tx);
                    let sid = sid.clone();
                    tokio::spawn(async move {
                        let local_track = Arc::new(TrackLocalStaticRTP::new(
                            track.codec().await.capability,
                            sid,
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

    // Waither for ice gathering to complete
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

    if let Some(local_track) = local_track_chan_rx.recv().await {
        let _ = ctx
            .tx
            .send(AppStateEv {
                action: "add-speaker-track".to_owned(),
                data: AppStateEvKind::StrTrack(data.id.clone(), local_track),
            })
            .await;
    };

    Ok(())
}

async fn speakerSendPeerConn(
    data: &str,
    tx: tokio::sync::mpsc::Sender<String>,
    ctx: Arc<Context>,
) -> Result<()> {
    let data = serde_json::from_str::<WSD>(data)?;
    let offer = data.sdp;

    let mut m = MediaEngine::default();
    let _ = m.register_codec(codec_parameters(), RTPCodecType::Audio);

    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut m)?;

    let api = APIBuilder::new()
        .with_media_engine(m)
        .with_interceptor_registry(registry)
        .build();

    let peer_conn = Arc::new(api.new_peer_connection(rtc_config()).await?);

    // let pc = Arc::downgrade(&peer_conn);

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

    // Waither for ice gathering to complete
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

    let _ = ctx
        .tx
        .send(AppStateEv {
            action: "add-speaker-conn".to_owned(),
            data: AppStateEvKind::StrConn(data.id.clone(), peer_conn),
        })
        .await;

    Ok(())
}

async fn listenerPeerConn(
    data: &String,
    mut tx: tokio::sync::mpsc::Sender<String>,
    ctx: Arc<Context>,
) -> Result<()> {
    Ok(())
}

async fn websocket(stream: WebSocket, state: Arc<Context>) {
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
                    "speakerSendOffer" => {
                        match speakerRecvPeerConn(&m.data, tx1.clone(), state.clone()).await {
                            Ok(_) => {}
                            Err(e) => {
                                eprintln!("speakerRecvPeerConn failed: {:?}", e);
                            }
                        }
                    }
                    "speakerRecvOffer" => {
                        match speakerSendPeerConn(&m.data, tx1.clone(), state.clone()).await {
                            Ok(_) => {}
                            Err(e) => {
                                eprintln!("speakerSendPeerConn failed: {:?}", e);
                            }
                        }
                    }
                    "listenerOffer" => {
                        match listenerPeerConn(&m.data, tx1.clone(), state.clone()).await {
                            Ok(_) => {}
                            Err(e) => {
                                eprintln!("listenerPeerConn failed: {:?}", e);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    });
}

async fn ws_handler(ws: WebSocketUpgrade, State(ctx): State<Arc<Context>>) -> impl IntoResponse {
    ws.on_upgrade(|socket| websocket(socket, ctx))
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut app_state = AppState::new();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<AppStateEv>(1);
    let ctx = Context { tx: tx.clone() };
    let app = axum::Router::with_state(Arc::new(ctx))
        .route("/", axum::routing::get(|| async { "Hello World" }))
        .route("/ws", axum::routing::get(ws_handler));

    tokio::spawn(async move {
        while let Some(data) = rx.recv().await {
            match data.action.as_str() {
                "app-speaker-track" => {
                    if let AppStateEvKind::StrTrack(id, track) = data.data {
                        let _ = app_state.add_speaker_track(id, track).await;
                    }
                }
                "app-speaker-conn" => {
                    if let AppStateEvKind::StrConn(id, conn) = data.data {
                        let _ = app_state.add_speaker_conn(id, conn).await;
                    }
                }
                _ => {}
            }
        }
    });

    axum::Server::bind(&"0.0.0.0:3000".parse().unwrap())
        .serve(app.into_make_service())
        .await?;

    Ok(())
}
