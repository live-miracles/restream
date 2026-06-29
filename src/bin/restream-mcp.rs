//! Standalone MCP binary scaffold for `restream`.

#[cfg(feature = "mcp-server")]
use std::sync::Arc;

#[cfg(feature = "mcp-server")]
fn usage() -> &'static str {
    "usage: restream-mcp [--print-tools] [--stdio] [--bind <addr>]"
}

#[cfg(not(feature = "mcp-server"))]
fn main() {
    eprintln!(
        "restream-mcp was built without the 'mcp-server' feature. Rebuild with --features mcp-server."
    );
    std::process::exit(1);
}

#[cfg(feature = "mcp-server")]
fn main() {
    let mut args = std::env::args().skip(1);
    let mut print_tools = false;
    let mut stdio = false;
    let mut bind_addr = "127.0.0.1:4040".to_string();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--print-tools" => print_tools = true,
            "--stdio" => stdio = true,
            "--bind" => {
                let Some(value) = args.next() else {
                    eprintln!("{}", usage());
                    std::process::exit(2);
                };
                bind_addr = value;
            }
            _ => {
                eprintln!("{}", usage());
                std::process::exit(2);
            }
        }
    }

    if print_tools {
        for tool in restream::agent_mcp::tool_catalog() {
            println!(
                "{}\tfeature={}\tcompiledIn={}",
                tool.name, tool.requires_feature, tool.compiled_in
            );
        }
        return;
    }

    #[cfg(not(feature = "mcp-http-backend"))]
    {
        eprintln!("restream-mcp requires the 'mcp-http-backend' feature in this scaffold to run.");
        std::process::exit(1);
    }

    #[cfg(feature = "mcp-http-backend")]
    {
        let base_url = std::env::var("RESTREAM_AGENT_BASE_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:3030".to_string());
        let auth = if let Ok(cookie) =
            std::env::var("RESTREAM_AGENT_SESSION_COOKIE").map(|s| s.trim().to_string())
        {
            if cookie.is_empty() {
                restream::agent_backends::http::HttpAgentAuth::None
            } else {
                restream::agent_backends::http::HttpAgentAuth::SessionCookie(cookie)
            }
        } else if let Ok(token) =
            std::env::var("RESTREAM_AGENT_BEARER_TOKEN").map(|s| s.trim().to_string())
        {
            if token.is_empty() {
                restream::agent_backends::http::HttpAgentAuth::None
            } else {
                restream::agent_backends::http::HttpAgentAuth::BearerToken(token)
            }
        } else {
            restream::agent_backends::http::HttpAgentAuth::None
        };
        let allowed_origins = std::env::var("RESTREAM_MCP_ALLOWED_ORIGINS")
            .ok()
            .map(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|origin| !origin.is_empty())
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let backend = restream::agent_backends::http::HttpAgentBackend::new(base_url, auth);
        let config = restream::agent_mcp::TransportConfig {
            mode: if stdio {
                restream::agent_mcp::TransportMode::Stdio
            } else {
                restream::agent_mcp::TransportMode::StreamableHttp
            },
            bind_addr,
            allowed_origins,
        };

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("Failed to build tokio runtime");
        let result = runtime.block_on(restream::agent_mcp::run_server(config, Arc::new(backend)));
        if let Err(error) = result {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
