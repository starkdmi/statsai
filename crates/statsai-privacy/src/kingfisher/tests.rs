use super::*;
use std::fs;
use std::fs::File;
use std::os::unix::fs::PermissionsExt;

#[test]
fn spawn_times_out_when_helper_never_becomes_ready() {
    let directory = tempfile::tempdir().unwrap();
    let helper = directory.path().join("unresponsive-helper");
    fs::write(&helper, "#!/bin/sh\nexec sleep 10\n").unwrap();
    let mut permissions = fs::metadata(&helper).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&helper, permissions).unwrap();

    let options = KingfisherOptions {
        startup_timeout: Duration::from_millis(50),
        shutdown_timeout: Duration::from_millis(50),
        ..KingfisherOptions::default()
    };
    let started = Instant::now();
    let error = KingfisherDetector::spawn(&helper, options)
        .err()
        .expect("unresponsive helper should fail startup");
    assert!(matches!(error, PrivacyError::Timeout));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn batches_respect_aggregate_byte_limit() {
    let ranges = request_ranges(&[MAX_SEQUENCE_BYTES; 5]).unwrap();
    assert_eq!(ranges, vec![0..4, 4..5]);
}

#[test]
fn oversized_utf8_fields_are_split_with_bounded_overlap() {
    let text = "é".repeat(MAX_SEQUENCE_BYTES / 2 + 8);
    let chunks = sequence_chunks(&[&text]).expect("chunk oversized UTF-8 field");

    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].range.start, 0);
    assert_eq!(chunks.last().map(|chunk| chunk.range.end), Some(text.len()));
    for chunk in &chunks {
        assert!(chunk.range.len() <= MAX_SEQUENCE_BYTES);
        assert!(text.is_char_boundary(chunk.range.start));
        assert!(text.is_char_boundary(chunk.range.end));
    }
    assert!(chunks[1].range.start < chunks[0].range.end);
    assert!(chunks[0].range.end - chunks[1].range.start >= SEQUENCE_OVERLAP_BYTES);
}

#[test]
fn chunk_findings_map_to_global_offsets_and_deduplicate_exact_overlap() {
    let text = "0123456789abcdefghij";
    let chunks = vec![
        SequenceChunk {
            text_index: 0,
            range: 0..14,
        },
        SequenceChunk {
            text_index: 0,
            range: 6..text.len(),
        },
    ];
    let finding = |start, end, confidence| DetectedSpan {
        start,
        end,
        category: PrivacyCategory::Secret,
        detector: DetectorKind::Kingfisher,
        confidence: Some(confidence),
    };
    let mut combined = vec![Vec::new()];

    append_chunk_results(
        &[text],
        &chunks,
        vec![
            vec![finding(8, 12, DetectionConfidence::High)],
            vec![
                finding(2, 6, DetectionConfidence::High),
                finding(2, 6, DetectionConfidence::Medium),
            ],
        ],
        &mut combined,
    )
    .expect("map chunk findings");
    normalize_chunk_results(&mut combined);

    assert_eq!(combined[0].len(), 2);
    assert!(combined[0]
        .iter()
        .all(|span| (span.start, span.end) == (8, 12)));
    assert!(combined[0]
        .iter()
        .any(|span| span.confidence == Some(DetectionConfidence::High)));
    assert!(combined[0]
        .iter()
        .any(|span| span.confidence == Some(DetectionConfidence::Medium)));
}

#[test]
fn response_span_limit_is_independent_of_request_bytes() {
    assert!(validate_response_dimensions(1, 1, 2).is_ok());
    assert!(validate_response_dimensions(1, 1, MAX_RESPONSE_SPANS).is_ok());
    assert!(matches!(
        validate_response_dimensions(1, 1, MAX_RESPONSE_SPANS + 1),
        Err(PrivacyError::Protocol(_))
    ));
}

#[test]
fn helper_identity_requires_exact_implementation_and_source_revision() {
    let expected = expected_helper_identity();
    assert_eq!(
        kingfisher_implementation_version(),
        "statsai-kingfisher/0.2.0; kingfisher/1.106.0"
    );
    assert!(validate_ping_header(0, 0, expected.len() as u32, expected.len()).is_ok());
    assert!(matches!(
        validate_ping_header(0, 0, 0, expected.len()),
        Err(PrivacyError::Protocol(_))
    ));
    assert!(validate_helper_identity(expected.as_bytes(), expected.as_bytes()).is_ok());

    for stale in [
        "statsai-kingfisher/0.1.0\nkingfisher/1.106.0\nrevision/8fa4f142bcd32664ac0feb16fc8aabc67637660d",
        "statsai-kingfisher/0.2.0\nkingfisher/1.105.0\nrevision/8fa4f142bcd32664ac0feb16fc8aabc67637660d",
        "statsai-kingfisher/0.2.0\nkingfisher/1.106.0\nrevision/0000000000000000000000000000000000000000",
    ] {
        assert!(matches!(
            validate_helper_identity(stale.as_bytes(), expected.as_bytes()),
            Err(PrivacyError::Protocol(_))
        ));
    }
}

#[test]
fn nonblocking_write_honors_timeout_when_pipe_is_full() {
    let (_reader, writer) = nix::unistd::pipe().unwrap();
    let mut writer = File::from(writer);
    set_nonblocking(&writer).unwrap();
    let payload = vec![0u8; MAX_SEQUENCE_BYTES];
    let started = Instant::now();
    let error = write_all_before(
        &mut writer,
        &payload,
        deadline_after(Duration::from_millis(50)),
    )
    .unwrap_err();
    assert!(matches!(error, PrivacyError::Timeout));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
#[ignore = "requires the separately built statsai-kingfisher helper"]
fn detects_secret_without_returning_secret_text() {
    let executable = std::env::var_os("STATSAI_KINGFISHER_HELPER")
        .expect("STATSAI_KINGFISHER_HELPER must point to the helper");
    let mut detector = KingfisherDetector::spawn(executable, KingfisherOptions::default()).unwrap();
    let metadata = detector.metadata();
    assert_eq!(
        metadata.implementation_version,
        "statsai-kingfisher/0.2.0; kingfisher/1.106.0"
    );
    assert_eq!(
        metadata.model_revision.as_deref(),
        Some(KINGFISHER_REVISION)
    );
    let token = ["ghp_", "EZopZDMWeildfoFzyH0KnWyQ5Yy3vy0Y2SU6"].concat();
    let texts = [format!("café token = {token}"), format!("token = {token}")];
    let text_refs = texts.iter().map(String::as_str).collect::<Vec<_>>();
    let detections = detector.detect_batch(&text_refs).unwrap();
    assert_eq!(detections.len(), 2);
    for (text, spans) in texts.iter().zip(detections) {
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].category, PrivacyCategory::Secret);
        assert_eq!(
            &text[spans[0].start..spans[0].end],
            "EZopZDMWeildfoFzyH0KnWyQ5Yy3vy"
        );
    }

    let secret = "EZopZDMWeildfoFzyH0KnWyQ5Yy3vy";
    let full_token = format!("{secret}0Y2SU6");
    let prefix = "x".repeat(MAX_SEQUENCE_BYTES - 20);
    let oversized = format!("{prefix}\ntoken = ghp_{full_token}");
    let detections = detector.detect(&oversized).unwrap();
    assert_eq!(detections.len(), 1);
    assert_eq!(&oversized[detections[0].start..detections[0].end], secret);
}
