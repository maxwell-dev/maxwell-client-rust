pub mod connection;
pub mod connection_mgr;
mod connection_pool;

pub mod prelude {
    //! The `maxwell-client` prelude.
    //!
    //! The purpose of this module is to alleviate imports of many common maxwell-client
    //! traits by adding a glob import to the top of maxwell-client heavy modules:
    //!
    //! ```
    //! use maxwell_client::prelude::*;
    //! ```
    pub use crate::{
        connection::{
            Connection, ConnectionStatusChangedMsg, ProtocolMsgWrapper, SendError, StopMsg,
            SubscribeConnectionStatusMsg, TimeoutExt, UnsubscribeConnectionStatusMsg, Wrap,
        },
        connection_mgr::ConnectionMgr,
    };
}
