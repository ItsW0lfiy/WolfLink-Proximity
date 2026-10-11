use async_trait::async_trait;
use bytes::BytesMut;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::time::timeout;
use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder,
    RTCIceGatheringState,
};

struct ProbeHandler {
    gathered: watch::Sender<bool>,
    incoming: Option<mpsc::Sender<Arc<dyn DataChannel>>>,
}

#[async_trait]
impl PeerConnectionEventHandler for ProbeHandler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.gathered.send(true);
        }
    }

    async fn on_data_channel(&self, channel: Arc<dyn DataChannel>) {
        if let Some(incoming) = &self.incoming {
            let _ = incoming.try_send(channel);
        }
    }
}

async fn wait_gathered(
    mut receiver: watch::Receiver<bool>,
) -> Result<(), Box<dyn std::error::Error>> {
    timeout(Duration::from_secs(10), async move {
        while !*receiver.borrow_and_update() {
            receiver.changed().await?;
        }
        Ok::<(), tokio::sync::watch::error::RecvError>(())
    })
    .await??;
    Ok(())
}

async fn wait_open(channel: &Arc<dyn DataChannel>) -> Result<(), Box<dyn std::error::Error>> {
    timeout(Duration::from_secs(10), async {
        loop {
            match channel.poll().await {
                Some(DataChannelEvent::OnOpen) => return Ok(()),
                Some(DataChannelEvent::OnClose) | None => return Err("data channel closed"),
                _ => {}
            }
        }
    })
    .await?
    .map_err(Into::into)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (offer_tx, offer_rx) = watch::channel(false);
    let (answer_tx, answer_rx) = watch::channel(false);
    let (incoming_tx, mut incoming_rx) = mpsc::channel(1);
    let offer = PeerConnectionBuilder::new()
        .with_configuration(RTCConfigurationBuilder::default().build())
        .with_handler(Arc::new(ProbeHandler {
            gathered: offer_tx,
            incoming: None,
        }))
        .with_udp_addrs(vec!["127.0.0.1:0"])
        .build()
        .await?;
    let answer = PeerConnectionBuilder::new()
        .with_configuration(RTCConfigurationBuilder::default().build())
        .with_handler(Arc::new(ProbeHandler {
            gathered: answer_tx,
            incoming: Some(incoming_tx),
        }))
        .with_udp_addrs(vec!["127.0.0.1:0"])
        .build()
        .await?;

    let outgoing = offer.create_data_channel("telemetry-probe", None).await?;
    offer
        .set_local_description(offer.create_offer(None).await?)
        .await?;
    wait_gathered(offer_rx).await?;
    let offer_sdp = offer.local_description().await.ok_or("missing offer SDP")?;
    answer.set_remote_description(offer_sdp).await?;
    answer
        .set_local_description(answer.create_answer(None).await?)
        .await?;
    wait_gathered(answer_rx).await?;
    let answer_sdp = answer
        .local_description()
        .await
        .ok_or("missing answer SDP")?;
    offer.set_remote_description(answer_sdp).await?;

    let incoming = timeout(Duration::from_secs(10), incoming_rx.recv())
        .await?
        .ok_or("no incoming data channel")?;
    wait_open(&outgoing).await?;
    wait_open(&incoming).await?;
    outgoing
        .send(BytesMut::from(&b"wolflink-loopback"[..]))
        .await?;
    let received = timeout(Duration::from_secs(10), async {
        loop {
            match incoming.poll().await {
                Some(DataChannelEvent::OnMessage(message)) => return Ok(message.data.to_vec()),
                Some(DataChannelEvent::OnClose) | None => return Err("data channel closed"),
                _ => {}
            }
        }
    })
    .await??;
    if received != b"wolflink-loopback" {
        return Err("unexpected loopback payload".into());
    }
    println!("WebRTC data-channel loopback received expected payload");
    offer.close().await?;
    answer.close().await?;
    Ok(())
}
