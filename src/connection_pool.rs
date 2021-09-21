use crate::connection::Connection;
use actix::prelude::*;
use dycovec::DycoVec;
use maxwell_utils::ArbiterPool;
use std::sync::{
    atomic::{AtomicU8, Ordering},
    Arc,
};

const MAX_SIZE: u8 = 16;

pub struct ConnectionPool {
    endpoint: String,
    connections: DycoVec<Arc<Addr<Connection>>>,
    index_seed: AtomicU8,
    arbiter_pool: Arc<ArbiterPool>,
}

impl ConnectionPool {
    pub fn new(endpoint: String, arbiter_pool: Arc<ArbiterPool>) -> Self {
        let connections = DycoVec::<Arc<Addr<Connection>>>::new();
        for _ in 0..MAX_SIZE {
            connections.push(Arc::new(Connection::start(
                endpoint.clone(),
                &arbiter_pool.fetch_arbiter().handle(),
            )));
        }
        ConnectionPool { endpoint, connections, index_seed: AtomicU8::new(0), arbiter_pool }
    }

    #[inline]
    pub fn fetch_connection(&mut self) -> Arc<Addr<Connection>> {
        let index_seed = self.index_seed.fetch_add(1, Ordering::Relaxed);
        let index = (index_seed % MAX_SIZE) as usize;
        let connection = &self.connections[index];
        if connection.connected() {
            Arc::clone(connection)
        } else {
            self.connections[index] = Arc::new(Connection::start(
                self.endpoint.clone(),
                &self.arbiter_pool.fetch_arbiter().handle(),
            ));
            Arc::clone(&self.connections[index])
        }
    }
}

////////////////////////////////////////////////////////////////////////////////
/// test cases
////////////////////////////////////////////////////////////////////////////////
#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicU8, Ordering},
        time::Instant,
    };

    #[actix::test]
    async fn test_fetch_add() {
        log4rs::init_file("config/log4rs.yaml", Default::default()).unwrap();
        let val = AtomicU8::new(0);
        let start = Instant::now();
        for _i in 0..258 {
            let next = val.fetch_add(1, Ordering::Relaxed);
            log::info!("next: {:?}", next);
        }
        let spent = Instant::now() - start;
        log::info!("Spent time: {:?}ms", spent.as_millis());
    }
}
