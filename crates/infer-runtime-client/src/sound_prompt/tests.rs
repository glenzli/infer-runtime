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
