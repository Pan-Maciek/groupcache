mod cache;

use crate::cache::{configure_groupcache, CachedValue};
use anyhow::Context;
use anyhow::Result;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::{Json, Router};
use groupcache::GroupcacheWrapper;
use k8s_openapi::api::core::v1::Pod;
use kube::api::ListParams;
use kube::{Api, Client};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::convert::Infallible;
use std::env;
use std::net::SocketAddr;
use std::time::Duration;
use tower::make::Shared;
use tower::steer::Steer;
use tower::ServiceExt;
use tower_http::classify::{ServerErrorsAsFailures, SharedClassifier};
use tower_http::trace::{DefaultMakeSpan, DefaultOnRequest, DefaultOnResponse, TraceLayer};
use tower_http::LatencyUnit;
use tracing::log::warn;
use tracing::{error, info, log, Level};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let pod_port = read_env("K8S_POD_PORT")?;
    let pod_ip = read_env("K8S_POD_IP")?;
    let pod_name = read_env("K8S_POD_NAME")?;
    let namespace = read_env("K8S_NAMESPACE")?;
    info!(
        r#" Running {}:
            K8S_POD_IP: {},
            K8S_POD_PORT: {},
            K8S_NAMESPACE: {},
        "#,
        pod_name, pod_ip, pod_port, namespace
    );

    let addr: SocketAddr = format!("{}:{}", pod_ip, pod_port).parse()?;

    // prometheus metrics
    let (prometheus_layer, metric_handle) = axum_prometheus::PrometheusMetricLayer::pair();

    // Groupcache instance, configured to respond to requests under `addr`
    let groupcache = configure_groupcache(addr).await?;

    // Example axum app with endpoints to retrieve values from groupcache.
    let axum_app = Router::new()
        .route("/", get(hello))
        .route("/root", get(hello))
        .route("/key/:key_id", get(get_key_handler))
        .with_state(groupcache.clone())
        .route("/metrics", get(|| async move { metric_handle.render() }))
        .route("/*fallback", any(handler_404))
        .layer(prometheus_layer)
        .layer(trace())
        .boxed_clone();

    // Groupcache gRPC service, used for cross-peer communication if there are multiple peers in the cluster.
    let grpc_groupcache = tonic::transport::Server::builder()
        .add_service(groupcache.grpc_service())
        .into_service()
        .map_response(|r| r.map(axum::body::boxed))
        .map_err::<_, Infallible>(|_| panic!("unreachable - make the compiler happy"))
        .boxed_clone();

    // Create a service that can respond to Web and gRPC
    let http_grpc = Steer::new(
        vec![axum_app, grpc_groupcache],
        |req: &Request<Body>, _svcs: &[_]| {
            let content_type = req.headers().get(CONTENT_TYPE).map(|v| v.as_bytes());
            usize::from(
                content_type == Some(b"application/grpc")
                    || content_type == Some(b"application/grpc+proto"),
            )
        },
    );

    let client = Client::try_default().await?;
    let pods_api: Api<Pod> = Api::default_namespaced(client);
    tokio::spawn(async move {
        let mut current_pods = Box::<HashSet<GroupcachePod>>::default();
        loop {
            let result = find_groupcache_pods(&pods_api, &pod_ip).await;
            let mut new_current_pods = HashSet::new();
            match result {
                Ok(pods) => {
                    for dead_pod in current_pods.difference(&pods) {
                        let pod_addr = dead_pod.addr;
                        let res = groupcache.remove_peer(pod_addr.into()).await;
                        match res {
                            Ok(_) => {
                                info!("Removed peer: {:?} from groupcache cluster", dead_pod);
                            }
                            Err(e) => {
                                warn!("Failed to remove peer from groupcache cluster: {}", e);
                            }
                        }
                    }

                    for new_pod in pods.difference(&current_pods) {
                        let pod_addr = new_pod.addr;
                        let res = groupcache.add_peer(pod_addr.into()).await;
                        match res {
                            Ok(_) => {
                                info!("Added peer: {:?} to groupcache cluster", new_pod);
                                new_current_pods.insert(new_pod.clone());
                            }
                            Err(e) => {
                                warn!(
                                    "Failed to add peer {:?} to groupcache cluster: {}",
                                    new_pod, e
                                );
                            }
                        }
                    }

                    for pod in pods.intersection(&current_pods) {
                        new_current_pods.insert(pod.clone());
                    }

                    *current_pods = new_current_pods;
                }
                Err(e) => {
                    warn!("Failed to refresh groupcache nodes: {}", e);
                }
            }

            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    });

    info!("Listening on addr: {}", addr);
    let bind_addr = format!("0.0.0.0:{}", pod_port).parse()?;
    axum::Server::bind(&bind_addr)
        .serve(Shared::new(http_grpc))
        .await
        .context("Failed to start axum server")?;

    Ok(())
}

#[derive(Eq, PartialEq, Hash, Debug, Clone)]
struct GroupcachePod {
    addr: SocketAddr,
}

fn read_env(env_var_name: &'static str) -> Result<String> {
    env::var(env_var_name).context(format!("Failed to read: '{}' env variable", env_var_name))
}

async fn find_groupcache_pods(pods_api: &Api<Pod>, my_ip: &str) -> Result<HashSet<GroupcachePod>> {
    let pods_with_label_query = ListParams::default().labels("app=groupcache-powered-backend");
    let pods = pods_api
        .list(&pods_with_label_query)
        .await?
        .into_iter()
        .filter_map(|pod| {
            let status = pod.status?;
            let pod_ip = status.pod_ip?;
            if pod_ip == my_ip {
                return None;
            }

            let Ok(ip) = pod_ip.parse() else {
                return None;
            };

            let addr = SocketAddr::new(ip, 3000);
            Some(GroupcachePod { addr })
        })
        .collect::<HashSet<_>>();

    Ok(pods)
}

fn trace() -> TraceLayer<SharedClassifier<ServerErrorsAsFailures>> {
    TraceLayer::new_for_http()
        .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
        .on_request(DefaultOnRequest::new().level(Level::INFO))
        .on_response(
            DefaultOnResponse::new()
                .level(Level::INFO)
                .latency_unit(LatencyUnit::Micros),
        )
}

#[derive(Serialize)]
struct GetResponse {
    key: String,
    value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GetResponseFailure {
    pub key: String,
    pub error: String,
}

async fn get_key_handler(
    Path(key): Path<String>,
    State(groupcache): State<GroupcacheWrapper<CachedValue>>,
) -> Response {
    log::info!("get_rpc_handler, {}!", key);

    match groupcache.get(&key).await {
        Ok(value) => {
            let value = value.plain_string;
            let response_body = GetResponse { key, value };
            (StatusCode::OK, Json(response_body)).into_response()
        }
        Err(error) => {
            error!("Received error from groupcache: {}", error.to_string());
            let response_body = GetResponseFailure {
                key,
                error: error.to_string(),
            };

            (StatusCode::INTERNAL_SERVER_ERROR, Json(response_body)).into_response()
        }
    }
}

async fn handler_404() -> impl IntoResponse {
    (StatusCode::NOT_FOUND, "nothing to see here\n")
}

async fn hello() -> &'static str {
    "Hello from groupcache-powered-backend-service!\n"
}
