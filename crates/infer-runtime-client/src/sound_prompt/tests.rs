use super::*;
#[tokio::test]
async fn english_is_byte_exact_without_discovery() {
    let c = Client::builder().build().unwrap();
    let p = c
        .prepare_sound_prompt("  Dry kick, 90 BPM, no vocals.  ")
        .await
        .unwrap();
    assert_eq!(p.effective_prompt, "  Dry kick, 90 BPM, no vocals.  ");
    assert!(p.text_job.is_none());
    p.validate_for(&p.original_prompt, "echo").unwrap();
    assert!(p.validate_for("Different", "echo").is_err());
    for bad in ["", "  ", "rain\nwind"] {
        assert!(c.prepare_sound_prompt(bad).await.is_err());
    }
}
#[test]
fn mixed_chinese_uses_exact_local_text_edit_request() {
    let r = request("古筝与 soft piano，80 BPM，不要人声");
    assert!(contains_chinese("古筝与 soft piano，80 BPM，不要人声"));
    assert_eq!(r.model, "text.edit");
    assert_eq!(r.metadata["infer.deployment_ids"], DEPLOYMENT);
    assert_eq!(r.metadata["infer.fallback"], "none");
    assert_eq!(r.metadata["infer.placement"], "local_only");
    assert!(r.tools.is_empty());
}
#[test]
fn malformed_or_chinese_effective_output_is_rejected() {
    let response = |text: &str| ResponsesResult {
        id: "job_1".into(),
        object: "response".into(),
        created_at: 0,
        model: "text.edit".into(),
        status: "completed".into(),
        output: vec![json!({"type":"message", "content":[{"type":"output_text","text":text}]})],
        extra: BTreeMap::new(),
    };
    for text in [
        "Translation: rain",
        "{\"effective_prompt\":\"下雨\"}",
        "{\"effective_prompt\":\"rain\",\"extra\":1}",
    ] {
        assert!(decode_response(&response(text)).is_err());
    }
    assert_eq!(
        decode_response(&response(
            "{\"effective_prompt\":\"Rain, no speech or music\"}"
        ))
        .unwrap(),
        "Rain, no speech or music"
    );
}

fn local_job() -> JobSnapshot {
    serde_json::from_value(json!({
        "id": "resp_prompt_fixture",
        "app_id": "shape",
        "intent": "text.edit",
        "consumer_core_contract": "infer-runtime.consumer-core@20260813.1",
        "capability_contract": "infer.responses@20260812.1",
        "provider": "ollama-local",
        "deployment": "ollama_qwen3_5_4b",
        "model_profile": "qwen3_5_4b",
        "model_build": "qwen3_5_4b_mlx",
        "physical_model": "qwen3.5:4b-mlx",
        "placement": "local",
        "capability_level": "foundational",
        "evaluation_status": "provisional",
        "resource_class": "standard",
        "state": "succeeded",
        "policy": "local-first",
        "priority": "background",
        "constraints": {
            "capability_floor": "foundational",
            "deadline_ms": null,
            "fallback": "none",
            "latency": "balanced",
            "max_cost_usd": 0.0,
            "named_route": {
                "kind": "deployment",
                "ordered_ids": [
                    "ollama_qwen3_5_4b"
                ]
            },
            "offline_required": true,
            "placement": "local_only",
            "policy": "local-first",
            "prefer": "local",
            "priority": "background",
            "provider_access_class": null
        },
        "routing": {
            "capability_floor": "foundational",
            "named_route": {
                "kind": "deployment",
                "ordered_ids": [
                    "ollama_qwen3_5_4b"
                ]
            },
            "candidates": [
                {
                    "deployment": "ollama_qwen3_5_4b",
                    "provider": "ollama-local",
                    "status": "eligible",
                    "rank": 1,
                    "reason_codes": []
                }
            ]
        },
        "attempts": [
            {
                "number": 1,
                "provider": "ollama-local",
                "deployment": "ollama_qwen3_5_4b",
                "outcome": "succeeded",
                "trigger": "initial",
                "error_kind": null,
                "error": null
            }
        ],
        "error": null
    }))
    .unwrap()
}

#[test]
fn restored_preparation_binds_original_app_and_local_job() {
    let prepared = PreparedSoundPrompt {
        original_prompt: "雨声，不要音乐".into(),
        effective_prompt: "Rain, no music".into(),
        rules_revision: SOUND_PROMPT_RULES_REVISION.into(),
        text_job: Some(local_job()),
        preparation_elapsed_ms: 100,
    };
    prepared.validate_for("雨声，不要音乐", "shape").unwrap();
    assert!(prepared.validate_for("雨声，不要音乐", "echo").is_err());
    assert!(prepared.validate_for("鼓声", "shape").is_err());
    let restored: PreparedSoundPrompt =
        serde_json::from_slice(&serde_json::to_vec(&prepared).unwrap()).unwrap();
    restored.validate_for("雨声，不要音乐", "shape").unwrap();
    let mutations: Vec<fn(&mut JobSnapshot)> = vec![
        |j| j.placement = "cloud".into(),
        |j| j.physical_model = "other".into(),
        |j| j.constraints["offline_required"] = json!(false),
        |j| j.constraints["fallback"] = json!("allow"),
        |j| j.constraints["named_route"]["ordered_ids"] = json!(["other"]),
        |j| {
            j.routing
                .named_route
                .as_mut()
                .unwrap()
                .ordered_ids
                .push("other".into())
        },
        |j| j.attempts[0].trigger = "fallback".into(),
        |j| j.error = Some("failed".into()),
    ];
    for change in mutations {
        let mut p = prepared.clone();
        change(p.text_job.as_mut().unwrap());
        assert!(p.validate_for("雨声，不要音乐", "shape").is_err());
    }
}

#[test]
fn recorded_music_regression_rejects_invented_music_exclusion() {
    let original = "轻柔的 guqin 和 sparse piano，舒缓节奏，不要人声，不要鼓点。";
    let mut prepared = PreparedSoundPrompt {
        original_prompt: original.into(),
        effective_prompt:
            "soft guqin and sparse piano, relaxed tempo, no speech, no music, no vocals, no drum beats"
                .into(),
        rules_revision: SOUND_PROMPT_RULES_REVISION.into(),
        text_job: Some(local_job()),
        preparation_elapsed_ms: 3042,
    };
    assert!(prepared.validate_for_generation(original, "shape").is_err());
    prepared.effective_prompt =
        "Soft guqin and sparse piano, relaxed tempo, no vocals, no drum beats.".into();
    prepared.validate_for_generation(original, "shape").unwrap();
}

#[test]
fn common_exclusions_must_be_present_in_the_source_and_preserved() {
    for (source, target) in [
        (
            "轻柔的钢琴音乐，不要人声",
            "Soft piano music, no vocals, no music",
        ),
        ("柔和的配乐，不要鼓点", "Soft music without music or drums"),
        ("雨声，不要讲话和音乐", "Rain, no speech"),
        ("古筝，不要鼓点", "Guzheng, no percussion"),
        ("钢琴，无缝循环", "Piano, no music"),
        ("不要去掉音乐", "No music"),
        ("No speech，但保留音乐", "No speech or music"),
    ] {
        assert!(
            !fidelity::preserves_exclusions(source, target),
            "{source} -> {target}"
        );
    }
    for (source, target) in [
        (
            "窗外雨声，不要讲话和音乐",
            "Rain outside, no speech or music",
        ),
        (
            "窗外雨声，没有音乐，没有人声",
            "Rain outside, without music or vocals",
        ),
        ("无音乐的雨声", "Rain, music-free"),
        (
            "雨声，不要有音乐，不要加入任何人声",
            "Rain, no music or voices",
        ),
        (
            "轻柔钢琴，不要人声，不要鼓点",
            "Soft piano, no vocals and no drum beats",
        ),
        ("安静的房间，不要打击乐", "Quiet room, no percussion"),
        ("轻柔的音乐，没有雨声", "Soft music, no rain"),
        ("无缝循环的音乐", "Seamless looping music"),
        ("不要去掉音乐", "Keep the music"),
    ] {
        assert!(
            fidelity::preserves_exclusions(source, target),
            "{source} -> {target}"
        );
    }
}

#[test]
fn historical_v1_evidence_is_readable_but_not_reusable_for_new_generation() {
    let original = "轻柔的音乐，不要人声";
    let legacy = PreparedSoundPrompt {
        original_prompt: original.into(),
        // Even a flawed historical translation is evidence of what ran. Do not
        // rewrite it or make an accepted audio project impossible to reopen.
        effective_prompt: "Soft music, no vocals, no music".into(),
        rules_revision: LEGACY_RULES_REVISION.into(),
        text_job: Some(local_job()),
        preparation_elapsed_ms: 50,
    };
    legacy.validate_for(original, "shape").unwrap();
    assert!(legacy.validate_for_generation(original, "shape").is_err());
}
