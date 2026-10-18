//! Consumer-scoped read-only text deployment discovery.
use super::{ApiError, ApiState, authenticate};
use axum::{Json, extract::State, http::HeaderMap};
use infer_control::ConsumerModelCatalog;
pub(super) async fn get(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Json<ConsumerModelCatalog>, ApiError> {
    let app = authenticate(&state, &headers)?;
    Ok(Json(state.runtime.consumer_model_catalog(&app).await?))
}

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        http::{Request, StatusCode, header},
    };
    use http_body_util::BodyExt;
    use infer_auth::AppCredentials;
    use infer_control::Runtime;
    use infer_core::{Modality, ProviderAccessClass, RuntimeConfig};
    use serde_json::Value;
    use std::collections::BTreeMap;
    use tower::ServiceExt;
    const TOKEN: &str = "model-catalog-test-token";
    fn service(subscription: bool, grant: bool) -> axum::Router {
        let mut config: RuntimeConfig =
            toml::from_str(include_str!("../../../config/infer.example.toml")).unwrap();
        let future = config.deployments["codex_gpt_6_sol"].clone();
        config
            .deployments
            .insert("future_dynamic_model".into(), future);
        let mut app = config.apps["local-operator"].clone();
        app.resource_admin = false;
        app.allow_all_speech_voice_aliases = false;
        app.allow_all_intents = false;
        app.allowed_intents = Some(vec!["language.respond".into(), "reasoning.solve".into()]);
        app.routing = Some(infer_core::AppRoutingConfig {
            named_deployment_ids: ["codex_gpt_6_sol".into()].into(),
            intents: BTreeMap::from([(
                "reasoning.solve".into(),
                infer_core::RoutingGrantConfig {
                    named_deployment_ids: ["codex_gpt_6_sol".into()].into(),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        });
        app.allowed_cloud_input_modalities.insert(Modality::Text);
        if subscription {
            app.allowed_provider_access_classes
                .insert(ProviderAccessClass::Subscription);
        } else {
            app.allowed_provider_access_classes
                .remove(&ProviderAccessClass::Subscription);
        }
        if grant {
            app.routing
                .as_mut()
                .unwrap()
                .intents
                .get_mut("reasoning.solve")
                .unwrap()
                .named_deployment_ids
                .insert("future_dynamic_model".into());
        }
        let mut observer = app.clone();
        observer.observer_access = infer_core::ObserverAccess::Summary;
        observer.allowed_intents = Some(vec![]);
        observer.routing = None;
        observer.allowed_builtin_tools.clear();
        observer.allow_agent_file_tasks = false;
        config.apps.insert("catalog-observer".into(), observer);
        config.apps.insert("pcp-runtime".into(), app);
        config.validate().unwrap();
        let credentials = AppCredentials::from_pairs([
            ("pcp-runtime", TOKEN),
            ("catalog-observer", "observer-test-token"),
        ])
        .unwrap();
        crate::router(Runtime::with_providers_and_credentials(
            config,
            BTreeMap::new(),
            credentials,
        ))
    }
    fn request(token: &str, contract: &str) -> Request<Body> {
        Request::get("/infer/v1/model-catalog")
            .header(
                crate::contract::CONSUMER_CORE_HEADER,
                crate::contract::CORE_CONTRACT,
            )
            .header(crate::contract::CAPABILITY_CONTRACT_HEADER, contract)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    }
    #[tokio::test]
    async fn model_catalog_is_dynamic_and_scoped_to_app_intent_and_provider_grants() {
        for (subscription, grant) in [(true, true), (true, false), (false, true)] {
            let response = service(subscription, grant)
                .oneshot(request(TOKEN, "infer.model-catalog@20261016.1"))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body: Value =
                serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            let models = body["deployments"].as_array().unwrap();
            let future = models
                .iter()
                .find(|m| m["deployment_id"] == "future_dynamic_model");
            assert_eq!(future.is_some(), subscription && grant);
            if let Some(future) = future {
                assert!(future["intents"].get("reasoning.solve").is_some());
                assert!(future["intents"].get("language.respond").is_none());
                assert!(
                    future["supported_efforts"]
                        .as_array()
                        .unwrap()
                        .contains(&Value::String("ultra".into()))
                );
            }
            let wire = body.to_string();
            for private in [
                "credential",
                "token",
                "api_key",
                "provider_endpoint",
                "max_concurrency",
            ] {
                assert!(!wire.contains(private), "{private}");
            }
        }
    }
    #[tokio::test]
    async fn model_catalog_requires_consumer_identity_and_exact_capability() {
        let app = service(true, true);
        assert_eq!(
            app.clone()
                .oneshot(request(
                    "observer-test-token",
                    "infer.model-catalog@20261016.1"
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            app.clone()
                .oneshot(request("invalid", "infer.model-catalog@20261016.1"))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            app.oneshot(request(TOKEN, "infer.model-catalog@20990101.1"))
                .await
                .unwrap()
                .status(),
            StatusCode::UPGRADE_REQUIRED
        );
    }
}
