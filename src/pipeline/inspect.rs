//! Credential-free inspection. Deliberately has no online mode or threshold search.
use super::RequestPipeline;
use crate::kiro::endpoint::{CliEndpoint, IdeEndpoint, KiroEndpoint, RequestContext};
use crate::kiro::model::credentials::KiroCredentials;
use crate::{
    anthropic::{converter::convert_request_with_pipeline, types::MessagesRequest},
    kiro::model::requests::kiro::KiroRequest,
    model::config::Config,
};
use serde_json::json;
use std::io::Read;

/// Apply known endpoint transformations without loading credentials or resolving a
/// profile. Final online profile injection remains explicitly outside this proof.
pub(super) fn endpoint_wire(body: &str, config: &Config) -> String {
    let credentials = KiroCredentials::default();
    let context = RequestContext {
        credentials: &credentials,
        token: "",
        machine_id: "",
        config,
    };
    match config.default_endpoint.as_str() {
        "cli" => CliEndpoint::new().transform_api_body(body, &context),
        _ => IdeEndpoint::new().transform_api_body(body, &context),
    }
}

pub fn run(config_path: &str, request_path: Option<&str>) -> anyhow::Result<()> {
    anyhow::ensure!(
        std::path::Path::new(config_path).is_file(),
        "configuration file does not exist: {config_path}"
    );
    let config = Config::load(config_path)?;
    config.request_pipeline.validate()?;
    let mut result = json!({"configurationValid":true,"networkRequests":0,"restartRequiredForChanges":true,"requestPipeline":config.request_pipeline});
    if let Some(path) = request_path {
        crate::model::custom_models::init(config.custom_models.clone());
        let limit = config.request_pipeline.ingress_max_bytes;
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes)?;
        anyhow::ensure!(
            bytes.len() <= limit,
            "local ingressMaxBytes exceeded; no network request made"
        );
        let mut payload: MessagesRequest = serde_json::from_slice(&bytes)?;
        let pipeline = RequestPipeline::new(config.request_pipeline.clone());
        let context = pipeline.prepare(&mut payload, 0)?;
        let converted = convert_request_with_pipeline(
            &payload,
            config.tool_compatibility_mode,
            &config.request_pipeline,
        )?;
        let request = KiroRequest {
            conversation_state: converted.conversation_state,
            profile_arn: None,
            additional_model_request_fields: converted.additional_model_request_fields,
        };
        let wire = endpoint_wire(
            &super::serialize_request(&payload, &request, &config.request_pipeline)?,
            &config,
        );
        result["audit"] =
            pipeline.audit(&wire, &config.default_endpoint, 0, &http::HeaderMap::new())?;
        result["stage"] = json!(
            "offline-endpoint-without-credentials; final profile/header inspection occurs on normal traffic only"
        );
        result["contextOffloaded"] = json!(context.is_some());
        result["nativeCacheEvidence"] = json!(null);
        result["localBudgetAccepted"] = json!(pipeline.preflight(&wire).is_ok());
        // Output measurements even when refused, then signal failure to automation.
        println!("{}", serde_json::to_string_pretty(&result)?);
        pipeline.preflight(&wire)?;
    } else {
        println!("{}", serde_json::to_string_pretty(&result)?);
    }
    Ok(())
}
