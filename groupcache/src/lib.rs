extern crate serde;
extern crate anyhow;
extern crate async_trait;
extern crate quick_cache;

use groupcache_pb::groupcache_pb::groupcache_server;
use std::error::Error;
use std::future::Future;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use serde::{Deserialize, Serialize};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use hashring::HashRing;
use tracing::{info, log};
use tracing::log::log;
use quick_cache::sync::Cache;
use tonic::{IntoRequest, Request, Status};


static VNODES_PER_PEER: i32 = 10;

#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
pub struct Peer {
    pub socket: SocketAddr,
}

#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
struct VNode {
    id: usize,
    addr: SocketAddr,
}

impl VNode {
    fn new(addr: SocketAddr, id: usize) -> Self {
        VNode {
            id,
            addr,
        }
    }

    fn vnodes_for_peer(peer: &Peer, num: i32) -> Vec<VNode> {
        let mut vnodes = Vec::new();
        for i in 0..num {
            let vnode = VNode::new(peer.socket.clone(), i as usize);
            vnodes.push(vnode);
        }
        vnodes
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetRequest {
    key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetResponse {
    key: String,
    value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetResponseFailure {
    pub key: String,
    pub error: String,
}

#[async_trait]
pub trait Transport: Send + Sync {
    async fn get_rpc(&self, peer: &Peer, req: &GetRequest) -> Result<GetResponse>;
}

pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl Transport for ReqwestTransport {
    async fn get_rpc(&self, peer: &Peer, req: &GetRequest) -> Result<GetResponse> {
        let addr = peer.socket.to_string();
        let response = self.client
            .get(format!("http://{}/get/{}", addr, req.key))
            .send()
            .await?;

        let status = response.status();
        if status != StatusCode::OK {
            let body = response.json::<GetResponseFailure>().await?;
            bail!("bad status code: {}, {:?}", status, body);
        }

        let response = response.json::<GetResponse>().await?;

        Ok((response))
    }
}


#[async_trait]
trait Retriever {
    async fn retrieve(&self, key: &str) -> Result<String>;
}

pub struct Groupcache {
    me: Peer,
    peers: RwLock<Vec<Peer>>,
    ring: Arc<RwLock<HashRing<VNode>>>,
    cache: Cache<Key, Value>,
    transport: Box<dyn Transport>,
}

type Value = Vec<u8>;
type Key = String;

#[async_trait]
impl groupcache_server::Groupcache for Groupcache {
    async fn get(&self, request: Request<groupcache_pb::groupcache_pb::GetRequest>) ->
    std::result::Result<tonic::Response<groupcache_pb::groupcache_pb::GetResponse>, Status> {
        let payload = request.into_inner();
        let v = format!("{}-v", payload.key.clone()).into_bytes();
        info!("get key:{}", payload.key);
        Ok(tonic::Response::new(groupcache_pb::groupcache_pb::GetResponse {
            value: Some(v)
        }))
    }
}


pub async fn start_grpc_server(
    groupcache: Arc<Groupcache>,
) -> Result<()> {
    let addr = groupcache.me.socket.clone();
    info!("Groupcache server listening on {}", addr);

    tonic::transport::Server::builder()
        .add_service(groupcache_server::GroupcacheServer::from_arc(groupcache))
        .serve(addr)
        .await?;

    Ok(())
}


impl Groupcache {
    pub fn new(me: Peer, transport: Box<dyn Transport>) -> Self {
        let ring = {
            let mut ring = HashRing::new();
            let vnodes = VNode::vnodes_for_peer(&me, VNODES_PER_PEER);
            for vnode in vnodes {
                ring.add(vnode)
            }

            Arc::new(RwLock::new(ring))
        };

        let cache = Cache::new(1_000_000);
        let peers = RwLock::new(vec![me.clone()]);

        Self {
            me,
            peers,
            ring,
            cache,
            transport,
        }
    }

    pub async fn get(&self, key: &Key) -> Result<Value> {
        if let Some(value) = self.cache.get(key) {
            return Ok(value);
        }

        let peer = {
            let lock = self.ring.read()
                .unwrap();

            let vnode = lock
                .get(&key)
                .context("no node found")?;
            Peer { socket: vnode.addr.clone() }
        };
        log::info!("peer {:?} getting from peer: {:?}", self.me.socket, peer.socket);

        let value = if peer == self.me {
            let value = vec![1, 2, 3];
            self.cache.insert(key.clone(), value.clone());
            value
        } else {
            // todo: call grpc endpoint
            // let GetResponse { key, value } = self.transport.get_rpc(&peer, &GetRequest {
            //     key: key.to_string(),
            // }).await.context("failed to retrieve kv from peer")?;

            vec![1, 2, 3]
        };

        Ok(value)
    }

    fn add_peer(&self, peer: Peer) -> Result<()> {
        if !self.peers.read().unwrap().contains(&peer) {
            self.peers.write().unwrap().push(peer.clone());
            let vnodes = VNode::vnodes_for_peer(&peer, VNODES_PER_PEER);
            let mut lock = self.ring.write().unwrap();
            for vnode in vnodes {
                lock.add(vnode);
            }
        } else {
            bail!("peer already exists");
        }

        Ok(())
    }
}

async fn add_peer_rpc_handler(
    Path(peer_address): Path<String>,
    State(groupcache): State<Arc<Groupcache>>,
) -> (StatusCode) {
    let Ok(socket) = peer_address.parse::<SocketAddr>() else {
        return StatusCode::BAD_REQUEST;
    };

    let Ok(_) = groupcache.add_peer(Peer { socket }) else {
        return StatusCode::INTERNAL_SERVER_ERROR;
    };

    StatusCode::OK
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {}
}
