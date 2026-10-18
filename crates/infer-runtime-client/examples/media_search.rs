//! cargo run -p infer-runtime-client --example media_search -- TOKEN_FILE QUERY KIND MEDIA_FILE MIME
use infer_runtime_client::{Client, MediaQueryEmbeddingRequest};
use std::{collections::BTreeMap, path::Path};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 5 {
        return Err(
            "usage: media_search TOKEN_FILE QUERY image|audio|video MEDIA_FILE MIME".into(),
        );
    }
    let client = Client::builder().credential_file(&args[0]).build()?;
    let metadata = BTreeMap::new();
    let query = client
        .embed_media_query(&MediaQueryEmbeddingRequest {
            model: "semantic.media_query".into(),
            text: args[1].clone(),
            query_revision: "example-query-v1".into(),
            metadata: metadata.clone(),
        })
        .await?;
    let mime = match args[4].as_str() {
        "image/jpeg" => "image/jpeg",
        "image/png" => "image/png",
        "audio/wav" => "audio/wav",
        "audio/flac" => "audio/flac",
        "audio/mpeg" => "audio/mpeg",
        "video/mp4" => "video/mp4",
        "video/webm" => "video/webm",
        _ => return Err("unsupported example MIME type".into()),
    };
    let path = Path::new(&args[3]);
    let media = match args[2].as_str() {
        "image" => {
            client
                .embed_media_image(path, mime, "example-source-v1", &metadata)
                .await?
        }
        "audio" => {
            client
                .embed_media_audio(path, mime, "example-source-v1", &metadata)
                .await?
        }
        "video" => {
            client
                .embed_media_video(path, mime, "example-source-v1", &metadata)
                .await?
        }
        _ => return Err("expected image, audio or video".into()),
    };
    if query.embedding_space != media.embedding_space
        || query.embedding.len() != media.embedding.len()
    {
        return Err("incompatible embedding spaces".into());
    }
    let similarity: f32 = query
        .embedding
        .iter()
        .zip(&media.embedding)
        .map(|(a, b)| a * b)
        .sum();
    println!(
        "{}",
        serde_json::json!({"space":media.embedding_space.identity,"cosine_similarity":similarity,"model_build":media.provenance.model_build,"query_job":query.id,"media_job":media.id})
    );
    Ok(())
}
