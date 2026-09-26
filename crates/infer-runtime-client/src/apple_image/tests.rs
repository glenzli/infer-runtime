use super::*;
fn request() -> AppleImageParameters {
    AppleImageParameters {
        model: "apple.aesthetics".into(),
        source_revision: "photo-edit-3".into(),
        options: AppleImageOperation::Aesthetics {},
        metadata: BTreeMap::new(),
    }
}
fn response() -> AppleImageResponse {
    serde_json::from_value(serde_json::json!({
    "id":"job-1", "source_revision":"photo-edit-3", "provider":"apple", "deployment":"apple_aesthetics", "model_build":"apple_aesthetics",
    "result":{"operation":"aesthetics", "overall_score":0.3, "is_utility":false},
    "provenance":{"os_version":"27.0", "worker_protocol":"infer.apple-image-worker@20260926.2", "worker_sha256":"a".repeat(64), "execution_location":"device", "execution_node":null, "elapsed_ms":12, "model_ownership":"apple_os_managed_weights_not_exposed", "request_revision":"1"}
})).unwrap()
}
#[test]
fn refuses_stale_wrong_operation_remote_and_unbounded_results() {
    let request = request();
    let mut r = response();
    r.validate_for(&request).unwrap();
    r.provenance.worker_protocol = "infer.apple-image-worker@20260926.1".into();
    assert!(r.validate_for(&request).is_err());
    r = response();
    r.source_revision = "stale".into();
    assert!(r.validate_for(&request).is_err());
    r.source_revision = request.source_revision.clone();
    r.provenance.execution_node = Some("other-mac".into());
    assert!(r.validate_for(&request).is_err());
    let mut private = request.clone();
    private
        .metadata
        .insert("infer.placement".into(), "private".into());
    r.validate_for(&private).unwrap();
    r.provenance.execution_node = None;
    r.result = AppleImageResult::Ocr {
        width: 1,
        height: 1,
        lines: vec![],
    };
    assert!(r.validate_for(&request).is_err());
    r.result = AppleImageResult::Aesthetics {
        overall_score: f32::NAN,
        is_utility: false,
    };
    assert!(r.validate_for(&request).is_err());
}
#[test]
fn refuses_cloud_and_invalid_prompts_before_io() {
    let mut r = request();
    r.validate().unwrap();
    r.metadata
        .insert("infer.placement".into(), "anywhere".into());
    assert!(r.validate().is_err());
    r.metadata.clear();
    r.options = AppleImageOperation::Segment {
        points: vec![],
        box_prompt: None,
    };
    assert!(r.validate().is_err());
}
#[test]
fn verifies_png_digest_geometry_and_raw_decoder() {
    let data = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a18sAAAAASUVORK5CYII=";
    let bytes = STANDARD.decode(data).unwrap();
    let mut raster = AppleImageRaster {
        width: 1,
        height: 1,
        content_type: "image/png".into(),
        data_base64: data.into(),
        sha256: format!("{:x}", Sha256::digest(&bytes)),
        semantics: "display_referred_srgb_8bit".into(),
    };
    assert_eq!(raster.png_bytes().unwrap(), bytes);
    raster.width = 2;
    assert!(raster.png_bytes().is_err());
    raster.width = 1;
    raster.sha256 = "0".repeat(64);
    assert!(raster.png_bytes().is_err());
    raster.sha256 = format!("{:x}", Sha256::digest(&bytes));
    let mut request = request();
    request.options = AppleImageOperation::RawRender {
        exposure: 0.0,
        noise_reduction: 1.0,
    };
    let mut r = response();
    for decoder in ["9", "9.dng", "8"] {
        r.result = AppleImageResult::RawRender {
            raster: raster.clone(),
            decoder_version: decoder.into(),
        };
        assert_eq!(r.validate_for(&request).is_ok(), decoder != "8");
    }
}

#[test]
fn rejects_removed_description_wire_types() {
    assert!(
        serde_json::from_str::<AppleImageOperation>(
            r#"{"operation":"describe","prompt":"What is here?"}"#
        )
        .is_err()
    );
    assert!(
        serde_json::from_str::<AppleImageResult>(
            r#"{"operation":"describe","text":"Unsupported"}"#
        )
        .is_err()
    );
}
