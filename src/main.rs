use axum::{body::Body, extract::Request, response::IntoResponse, response::Response};
use std::fs::File;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use url::Url;

use http::uri::Authority;
use http::Uri;
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server;
use tokio::net::TcpListener;
use tower::Service;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use structopt::StructOpt;

#[derive(Debug, StructOpt)]
struct Opt {
    /// Listen address
    #[structopt(short, long)]
    listen: String,

    /// Target host
    #[structopt(short, long)]
    target: String,

    /// User-agent
    #[structopt(short, long)]
    user_agent: Option<String>,

    /// Trace requests
    #[structopt(long)]
    trace: bool,

    /// Access log file
    #[structopt(short, long)]
    access_log_file: Option<String>,
}

/// Proxy to a TLS service with reduced capabilities
#[derive(Clone)]
struct Proxy {
    /// Target authority (with port and without scheme)
    target_authority: Authority,
    /// Target hostname (with port and scheme)
    target_host: String,
    /// Pre-configured client for a target service
    client: reqwest::Client,
    /// Access log logger
    access_log_layer: Arc<Mutex<Option<File>>>,
}

impl Proxy {
    pub fn new(
        target: String,
        user_agent: Option<String>,
        trace: bool,
        access_log_layer: Arc<Mutex<Option<File>>>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Compose target host
        let target_host = Self::get_target_host(&target)?;
        let target_authority = target_host.parse()?;
        assert!(!target_host.starts_with("http"));
        let target_host_tls: String = "https://".to_owned() + &target_host;

        // Prepopulate headers
        let mut headers = http::HeaderMap::with_capacity(2);
        headers.insert("Referer", target_host_tls.parse().unwrap());
        headers.append("Origin", target_host_tls.parse().unwrap());
        if let Some(value) = user_agent {
            headers.append("User-Agent", value.parse().unwrap());
        }
        tracing::info!("Using headers: {:?}", &headers);

        // Precreate a TLS client
        // - HTTP1-only
        // - no TLS verification
        // - Special case for headers
        let client = reqwest::ClientBuilder::new()
            .http1_only()
            .http1_title_case_headers()
            .danger_accept_invalid_certs(true)
            .default_headers(headers.clone())
            .connection_verbose(trace)
            .build()?;
        Ok(Self {
            client,
            target_authority,
            target_host,
            access_log_layer,
        })
    }

    /// Get target host without scheme
    fn get_target_host(target: &str) -> Result<String, Box<dyn std::error::Error>> {
        let target_uri =
            Url::parse(target).map_err(|err| format!("Cannot parse target host: {}", err))?;
        let target_host = target_uri
            .host_str()
            .ok_or("Cannot parse target host: {}")?;
        Ok(target_host.into())
    }

    fn compose_target_uri(&self, source_uri: &Uri) -> Result<Uri, Box<dyn std::error::Error>> {
        let target_uri: Uri = Uri::builder()
            .scheme("https")
            .authority(self.target_authority.clone())
            .path_and_query(source_uri.to_string())
            .build()
            .map_err(|err| format!("Cannot build target url: {}", err))?;
        Ok(target_uri)
    }

    async fn serve_request(
        &self,
        client_addr: SocketAddr,
        req: Request,
    ) -> Result<Response, Box<dyn std::error::Error>> {
        let target_uri: Uri = self
            .compose_target_uri(req.uri())
            .map_err(|err| format!("Wrong target URI {} with err={}", &self.target_host, err))?;

        let method = req.method().clone();

        self.log_access_log(&client_addr, &req)?;
        // Pass the request stream directly to the target service
        let request_body_stream = req.into_body().into_data_stream();
        let request_body = reqwest::Body::wrap_stream(request_body_stream);
        let response = self
            .client
            .request(method, target_uri.to_string())
            .body(request_body)
            .send()
            .await?;
        let response_headers = response.headers().clone();
        let response_bytes = response.bytes().await?;
        let mut proxy_response: Response = Response::new(response_bytes.into());
        if let Some(header_value) = response_headers.get("Content-Type") {
            proxy_response
                .headers_mut()
                .insert("Content-Type", header_value.clone());
        }
        Ok(proxy_response)
    }

    fn log_access_log(
        &self,
        client_addr: &SocketAddr,
        req: &Request,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // 116.35.41.41 - - [21/May/2022:11:22:41 +0000] "GET /aboutus.html HTTP/1.1" 200 6430 "http://34.227.9.153/" "UA"
        let mut locked = self.access_log_layer.lock().unwrap();

        if locked.is_none() {
            return Ok(());
        }
        let mut file: &File = locked.as_mut().unwrap();
        let remote_addr = client_addr.to_string();
        let time = chrono::Utc::now()
            .format("[%d/%b/%Y:%H:%M:%S %z]")
            .to_string();
        let line = format!(
            "{} - - {} \"{} {} {:?}\" 0 0\n",
            remote_addr,
            time,
            req.method(),
            req.uri(),
            req.version(),
        );
        file.write_all(line.as_bytes())?;
        Ok(())
    }

    async fn retls(&self, client_addr: SocketAddr, req: Request) -> Result<Response, hyper::Error> {
        tracing::trace!(?req);

        match self.serve_request(client_addr, req).await {
            Ok(response) => Ok(response),
            Err(err) => {
                // Serve 404 on any error
                tracing::error!(err);
                Ok((http::StatusCode::BAD_REQUEST, "Error serving request").into_response())
            }
        }
    }
}

#[tokio::main]
async fn main() {
    let opt = Opt::from_args();

    let access_file = opt.access_log_file.map(|access_log_filename| {
        File::create(access_log_filename).expect("Cannot write access log")
    });
    let access_log_rc = Arc::new(Mutex::new(access_file));

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive(tracing_subscriber::filter::LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Create a retls proxy
    let proxy = Proxy::new(opt.target, opt.user_agent, opt.trace, access_log_rc).unwrap();

    tracing::info!("listening on {}", &opt.listen);

    let listener = TcpListener::bind(opt.listen).await.unwrap();
    loop {
        let (stream, addr) = listener.accept().await.unwrap();
        tracing::info!("Connection from {}", addr);
        let io = TokioIo::new(stream);

        let proxy = proxy.clone();
        let tower_service = tower::service_fn(move |req: Request<_>| {
            let proxy = proxy.clone();
            let req = req.map(Body::new);
            async move { proxy.retls(addr, req).await }
        });

        let hyper_service = hyper::service::service_fn(move |request: Request<Incoming>| {
            tower_service.clone().call(request)
        });

        tokio::task::spawn(async move {
            if let Err(err) = server::conn::auto::Builder::new(TokioExecutor::new())
                .serve_connection_with_upgrades(io, hyper_service)
                .await
            {
                tracing::error!("Failed to serve connection: {:?}", err);
            }
        });
    }
}
