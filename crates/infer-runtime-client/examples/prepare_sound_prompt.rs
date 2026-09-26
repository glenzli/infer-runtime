//! prepare_sound_prompt CREDENTIAL_FILE APP_ID PROMPT OUTPUT.json
use infer_runtime_client::Client;
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 4 || !std::path::Path::new(&args[3]).is_absolute() {
        return Err(
            "usage: prepare_sound_prompt CREDENTIAL_FILE APP_ID PROMPT ABSOLUTE_OUTPUT.json".into(),
        );
    }
    let client = Client::builder().credential_file(&args[0]).build()?;
    let prepared = client.prepare_sound_prompt(&args[2]).await?;
    prepared.validate_for(&args[2], &args[1])?;
    std::fs::write(&args[3], serde_json::to_vec_pretty(&prepared)?)?;
    println!("{}", args[3]);
    Ok(())
}
