use crate::{connection::Connection, connection_pool::ConnectionPool};
use actix::prelude::*;
use ahash::RandomState;
use dashmap::DashMap;
use maxwell_utils::ArbiterPool;
use std::sync::Arc;

pub struct ConnectionMgr {
    mappings: DashMap<String, ConnectionPool, RandomState>,
    arbiter_pool: Arc<ArbiterPool>,
}

impl ConnectionMgr {
    pub fn new(arbiter_pool: Arc<ArbiterPool>) -> Self {
        ConnectionMgr {
            mappings: DashMap::with_capacity_and_hasher(512, RandomState::new()),
            arbiter_pool,
        }
    }

    #[inline]
    pub fn fetch_connection(&self, endpoint: &str) -> Arc<Addr<Connection>> {
        self.mappings
            .entry(endpoint.to_owned())
            .or_insert_with(|| {
                ConnectionPool::new(endpoint.to_owned(), Arc::clone(&self.arbiter_pool))
            })
            .value_mut()
            .fetch_connection()
    }
}

////////////////////////////////////////////////////////////////////////////////
/// test cases
////////////////////////////////////////////////////////////////////////////////
#[cfg(test)]
mod tests {
    use crate::{connection::Connection, connection_mgr::ConnectionMgr, prelude::Wrap};
    use actix::prelude::*;
    use maxwell_protocol::{IntoProtocol, PingReq};
    use maxwell_utils::ArbiterPool;
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };
    use tokio::time::sleep;

    #[actix::test]
    async fn fetch_connection() {
        log4rs::init_file("config/log4rs.yaml", Default::default()).unwrap();
        let connection_mgr = ConnectionMgr::new(Arc::new(ArbiterPool::new()));
        let endpoint = "localhost:8081";
        let mut connections: Vec<Arc<Addr<Connection>>> = Vec::new();
        let start = Instant::now();
        for _i in 0..32 {
            let connection = connection_mgr.fetch_connection(&endpoint);
            connection.send(PingReq { r#ref: 1 }.into_protocol().wrap()).await.unwrap().unwrap();
            connections.push(connection);
        }
        sleep(Duration::from_secs(3)).await;
        let spent = Instant::now() - start;
        log::info!("Spent time: {:?}ms", spent.as_millis());
    }
}
