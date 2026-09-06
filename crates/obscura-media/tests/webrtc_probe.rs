#![cfg(feature = "webrtc-probe")]

use std::path::PathBuf;

use obscura_host_protocol::{
    WEBRTC_CONTROL_LABEL, WEBRTC_PROTOCOL_VERSION, WebRtcCapability, WebRtcNetworkPath,
    WebRtcSessionBinding, WebRtcTransport, WebRtcVideoCodec,
};
use obscura_media::webrtc::{WebRtcProbeInput, run_local_webrtc_probe};

fn probe_input() -> WebRtcProbeInput {
    let width: u32 = 64;
    let height: u32 = 48;
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            rgba.extend_from_slice(&[(x * 4) as u8, (y * 5) as u8, ((x + y) * 3) as u8, 255]);
        }
    }
    let binding = WebRtcSessionBinding {
        session_id: "probe-session-1".to_owned(),
        attachment_id: 7,
        auth_binding: b"probe-auth-binding".to_vec(),
    };
    WebRtcProbeInput {
        ffmpeg: PathBuf::from("ffmpeg"),
        width,
        height,
        rgba,
        capability: WebRtcCapability {
            protocol_version: WEBRTC_PROTOCOL_VERSION,
            transport: WebRtcTransport::Udp,
            video_codec: WebRtcVideoCodec::H264AnnexB,
            data_channel_label: WEBRTC_CONTROL_LABEL.to_owned(),
            max_access_unit: 4 * 1024 * 1024,
        },
        offerer_binding: binding.clone(),
        answerer_binding: binding,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_probe_proves_udp_rtp_decode_and_control_roundtrip() {
    let report = run_local_webrtc_probe(probe_input())
        .await
        .expect("real local WebRTC probe should pass");
    println!(
        "{}",
        serde_json::to_string_pretty(&report).expect("probe report serializes")
    );
    assert!(!report.product_enabled);
    assert_eq!(report.network_path, WebRtcNetworkPath::Local);
    assert_eq!(report.transport, WebRtcTransport::Udp);
    assert_eq!(report.local_candidate.protocol, WebRtcTransport::Udp);
    assert_eq!(report.remote_candidate.protocol, WebRtcTransport::Udp);
    assert_eq!(report.local_candidate.candidate_type, "host");
    assert_eq!(report.remote_candidate.candidate_type, "host");
    assert!(report.local_candidate.port > 0);
    assert!(report.remote_candidate.port > 0);
    assert_eq!(report.data_channel.label, WEBRTC_CONTROL_LABEL);
    assert!(report.data_channel.hello_ack);
    assert!(report.data_channel.ping_pong);
    assert!(report.frame.rtp_packets_received > 0);
    assert!(report.frame.access_unit_bytes > 0);
    assert_eq!(report.frame.decoded_rgba_bytes, 64 * 48 * 4);
    assert!(report.frame.decoded_nonzero);
    assert_ne!(report.frame.decoded_rgba_checksum, 0);
}

#[tokio::test]
async fn local_probe_rejects_auth_binding_mismatch_before_transport_setup() {
    let mut input = probe_input();
    input.answerer_binding.auth_binding = b"different-binding".to_vec();
    let error = run_local_webrtc_probe(input)
        .await
        .expect_err("auth mismatch must fail explicitly");
    assert!(error.to_string().contains("auth/session binding mismatch"));
}

#[tokio::test]
async fn local_probe_rejects_codec_and_version_mismatch_before_transport_setup() {
    let mut codec_input = probe_input();
    codec_input.capability.video_codec = WebRtcVideoCodec::H265AnnexB;
    let codec_error = run_local_webrtc_probe(codec_input)
        .await
        .expect_err("codec mismatch must fail explicitly");
    assert!(codec_error.to_string().contains("H.264 Annex B"));

    let mut version_input = probe_input();
    version_input.capability.protocol_version += 1;
    let version_error = run_local_webrtc_probe(version_input)
        .await
        .expect_err("protocol version mismatch must fail explicitly");
    assert!(
        version_error
            .to_string()
            .contains("capability protocol version")
    );
}
