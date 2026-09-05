use obscura_media::encode;

#[tokio::test]
async fn h264_is_independently_decodable_with_odd_viewport() {
    let pixels = [255, 0, 0, 255].repeat(65 * 47);
    let encoded = encode(std::path::Path::new("ffmpeg"), 65, 47, pixels).await.unwrap();
    let mut decoder = tokio::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-f", "h264", "-i", "pipe:0", "-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgb24", "pipe:1"])
        .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped()).kill_on_drop(true).spawn().unwrap();
    use tokio::io::AsyncWriteExt;
    let mut input = decoder.stdin.take().unwrap();
    input.write_all(&encoded).await.unwrap(); drop(input);
    let output = tokio::time::timeout(std::time::Duration::from_secs(5), decoder.wait_with_output()).await.unwrap().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.stdout.len(), 66 * 48 * 3);
    let center = &output.stdout[(20 * 66 + 20) * 3..][..3];
    assert!(center[0] > 220 && center[1] < 30 && center[2] < 30, "{center:?}");
}

#[tokio::test]
async fn invalid_pixels_and_missing_encoder_fail_explicitly() {
    assert!(encode(std::path::Path::new("ffmpeg"), 0, 1, vec![]).await.is_err());
    assert!(encode(std::path::Path::new("ffmpeg"), 2, 2, vec![0; 12]).await.is_err());
    assert!(encode(std::path::Path::new("/nonexistent-obscura-encoder"), 2, 2, vec![0; 16]).await.is_err());
}
