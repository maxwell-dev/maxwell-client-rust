use actix::{
    prelude::{Request, *},
    Addr, Message as ActixMessage,
};
use actix_codec::Framed;
use actix_web_actors::ws::{Frame, Message as WSMessage};
use awc::{error::WsProtocolError, ws::Codec, BoxedSocket, Client};
use futures::future::{AbortHandle, Abortable};
use futures_intrusive::sync::LocalManualResetEvent;
use futures_util::{
    sink::SinkExt,
    stream::{SplitSink, SplitStream, StreamExt},
};
use maxwell_protocol::{self, ProtocolMsg, *};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    future::Future,
    pin::Pin,
    rc::Rc,
    sync::atomic::{AtomicU32, Ordering},
    task::{Context as TaskContext, Poll, Waker},
    time::Duration,
};
use tokio::time::{sleep, timeout};

static ID_SEED: AtomicU32 = AtomicU32::new(0);

struct RoundAttachment {
    response: Option<ProtocolMsg>,
    waker: Option<Waker>,
}

struct RoundCompleter {
    round_ref: u32,
    connection_inner: Rc<ConnectionInner>,
}

impl RoundCompleter {
    fn new(round_ref: u32, connection_inner: Rc<ConnectionInner>) -> Self {
        connection_inner
            .round_attachments
            .borrow_mut()
            .insert(round_ref, RoundAttachment { response: None, waker: None });
        RoundCompleter { round_ref, connection_inner }
    }
}

impl Drop for RoundCompleter {
    fn drop(&mut self) {
        self.connection_inner.round_attachments.borrow_mut().remove(&self.round_ref);
    }
}

impl Future for RoundCompleter {
    type Output = ProtocolMsg;

    fn poll(self: Pin<&mut Self>, ctx: &mut TaskContext<'_>) -> Poll<ProtocolMsg> {
        let mut round_attachments = self.connection_inner.round_attachments.borrow_mut();
        let round_attachment = round_attachments.get_mut(&self.round_ref).unwrap();
        if let Some(msg) = round_attachment.response.take() {
            Poll::Ready(msg)
        } else {
            match round_attachment.waker.as_ref() {
                None => {
                    round_attachment.waker = Some(ctx.waker().clone());
                }
                Some(waker) => {
                    if !waker.will_wake(ctx.waker()) {
                        round_attachment.waker = Some(ctx.waker().clone());
                    }
                }
            }
            Poll::Pending
        }
    }
}

struct ConnectionInner {
    id: u32,
    url: String,
    sink: RefCell<Option<SplitSink<Framed<BoxedSocket, Codec>, WSMessage>>>,
    stream: RefCell<Option<SplitStream<Framed<BoxedSocket, Codec>>>>,
    connected_event: LocalManualResetEvent,
    disconnected_event: LocalManualResetEvent,
    is_connected: Cell<bool>,
    round_attachments: RefCell<HashMap<u32, RoundAttachment>>,
    round_ref: Cell<u32>,
    subscribers: RefCell<HashSet<Recipient<ConnectionStatusChangedMsg>>>,
    is_stopping: Cell<bool>,
}

impl ConnectionInner {
    #[inline]
    pub fn new(endpoint: String) -> Self {
        ConnectionInner {
            id: ID_SEED.fetch_add(1, Ordering::Relaxed),
            url: Self::build_url(&endpoint),
            sink: RefCell::new(None),
            stream: RefCell::new(None),
            connected_event: LocalManualResetEvent::new(false),
            disconnected_event: LocalManualResetEvent::new(true),
            is_connected: Cell::new(false),
            round_attachments: RefCell::new(HashMap::new()),
            round_ref: Cell::new(0),
            subscribers: RefCell::new(HashSet::new()),
            is_stopping: Cell::new(false),
        }
    }

    pub async fn connect_repeatedly(self: Rc<Self>) {
        loop {
            if self.is_stopping() {
                break;
            }

            self.disconnected_event.wait().await;

            log::info!("Connecting: actor: {}<{}>", &self.url, &self.id);
            match Client::new().ws(&self.url).connect().await {
                Ok((_resp, socket)) => {
                    log::info!("Connected: actor: {}<{}>", &self.url, &self.id);
                    let (sink, stream) = StreamExt::split(socket);
                    self.set_socket_pair(Some(sink), Some(stream));
                    self.toggle_to_connected();
                }
                Err(err) => {
                    log::error!(
                        "Failed to connect: actor: {}<{}>, err: {}",
                        &self.url,
                        &self.id,
                        err
                    );
                    self.set_socket_pair(None, None);
                    self.toggle_to_disconnected();
                    sleep(Duration::from_millis(1000)).await;
                }
            }
        }
    }

    pub async fn send(self: Rc<Self>, mut msg: ProtocolMsg) -> Result<ProtocolMsg, SendError> {
        let round_ref = self.next_round_ref();
        let round_completer = RoundCompleter::new(round_ref, Rc::clone(&self));
        let msg = maxwell_protocol::set_round_ref(&mut msg, round_ref);

        if !self.is_connected() {
            self.connected_event.wait().await;
        }

        if let Err(err) =
            self.sink.borrow_mut().as_mut().unwrap().send(WSMessage::Binary(encode(msg))).await
        {
            log::error!("Protocol error occured: err: {}", &err);
            return Err(SendError::ProtocolError(err));
        }

        Ok(round_completer.await)
    }

    pub async fn receive_repeatedly(self: Rc<Self>) {
        loop {
            if self.is_stopping() {
                break;
            }

            if !self.is_connected() {
                self.connected_event.wait().await;
            }

            if let Some(res) = self.stream.borrow_mut().as_mut().unwrap().next().await {
                match res {
                    Ok(frame) => match frame {
                        Frame::Binary(bytes) => {
                            let response = maxwell_protocol::decode(&bytes).unwrap();
                            let round_ref = maxwell_protocol::get_round_ref(&response);
                            let mut round_attachments = self.round_attachments.borrow_mut();
                            if let Some(attachment) = round_attachments.get_mut(&round_ref) {
                                attachment.response = Some(response);
                                attachment.waker.as_ref().unwrap().wake_by_ref();
                            }
                        }
                        Frame::Ping(_) | Frame::Pong(_) => {}
                        Frame::Close(reason) => {
                            log::error!(
                                "Disconnected: actor: {}<{}>, err: {:?}",
                                &self.url,
                                &self.id,
                                &reason
                            );
                            self.toggle_to_disconnected();
                        }
                        other => {
                            log::warn!("Received unknown msg: {:?}", &other);
                        }
                    },
                    Err(err) => {
                        log::error!("Protocol error occured: err: {}", &err);
                        self.toggle_to_disconnected();
                    }
                }
            } else {
                log::error!(
                    "Disconnected: actor: {}<{}>, err: {}",
                    &self.url,
                    &self.id,
                    "eof of the stream"
                );
                self.toggle_to_disconnected();
            }
        }
    }

    #[inline]
    pub fn stop(&self) {
        self.is_stopping.set(true);
    }

    #[inline]
    pub fn subscribe(&self, r: Recipient<ConnectionStatusChangedMsg>) {
        self.subscribers.borrow_mut().insert(r);
    }

    #[inline]
    pub fn unsubscribe(&self, r: Recipient<ConnectionStatusChangedMsg>) {
        self.subscribers.borrow_mut().remove(&r);
    }

    #[inline]
    fn build_url(endpoint: &str) -> String {
        format!("ws://{}", endpoint)
    }

    #[inline]
    fn set_socket_pair(
        &self, sink: Option<SplitSink<Framed<BoxedSocket, Codec>, WSMessage>>,
        stream: Option<SplitStream<Framed<BoxedSocket, Codec>>>,
    ) {
        *self.sink.borrow_mut() = sink;
        *self.stream.borrow_mut() = stream;
    }

    #[inline]
    fn toggle_to_connected(&self) {
        self.is_connected.set(true);
        self.connected_event.set();
        self.disconnected_event.reset();
        self.notify(ConnectionStatusChangedMsg::Connected);
    }

    #[inline]
    fn toggle_to_disconnected(&self) {
        self.is_connected.set(false);
        self.connected_event.reset();
        self.disconnected_event.set();
        self.notify(ConnectionStatusChangedMsg::Disconnected);
    }

    #[inline]
    fn is_connected(&self) -> bool {
        self.is_connected.get()
    }

    #[inline]
    fn is_stopping(&self) -> bool {
        self.is_stopping.get()
    }

    #[inline]
    fn next_round_ref(&self) -> u32 {
        let round_ref = self.round_ref.get();
        if round_ref < 1000000 {
            self.round_ref.set(round_ref + 1);
        } else {
            self.round_ref.set(1);
        }
        self.round_ref.get()
    }

    #[inline]
    fn notify(&self, status: ConnectionStatusChangedMsg) {
        let mut unavailables: Vec<Recipient<ConnectionStatusChangedMsg>> = Vec::new();
        for s in &*self.subscribers.borrow() {
            if s.connected() {
                s.do_send(status.clone()).unwrap_or_else(|err| {
                    log::warn!("Failed to notify: err: {:?}", &err);
                });
            } else {
                unavailables.push(s.clone());
            }
        }
        for s in &unavailables {
            self.subscribers.borrow_mut().remove(s);
        }
    }
}

pub struct Connection {
    inner: Rc<ConnectionInner>,
}

impl Connection {
    #[inline]
    pub fn new(endpoint: String) -> Self {
        Connection { inner: Rc::new(ConnectionInner::new(endpoint)) }
    }

    #[inline]
    pub fn start(endpoint: String, arbiter_handle: &ArbiterHandle) -> Addr<Self> {
        Connection::start_in_arbiter(arbiter_handle, move |_ctx| Connection::new(endpoint))
    }

    #[inline]
    pub async fn stop(addr: Addr<Self>) -> Result<(), MailboxError> {
        addr.send(StopMsg).await
    }
}

impl Actor for Connection {
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        log::info!("Started: actor: {}<{}>", &self.inner.url, &self.inner.id);
        ctx.spawn(Box::pin(Rc::clone(&self.inner).connect_repeatedly().into_actor(self)));
        ctx.spawn(Box::pin(Rc::clone(&self.inner).receive_repeatedly().into_actor(self)));
    }

    fn stopping(&mut self, _: &mut Self::Context) -> Running {
        log::info!("Stopping: actor: {}<{}>", &self.inner.url, &self.inner.id);
        self.inner.stop();
        Running::Stop
    }

    fn stopped(&mut self, _: &mut Context<Self>) {
        log::info!("Stopped: actor: {}<{}>", &self.inner.url, &self.inner.id);
    }
}

#[derive(Debug)]
pub enum SendError {
    Timeout,
    MailboxClosed,
    ProtocolError(WsProtocolError),
}

#[derive(Debug, ActixMessage)]
#[rtype(result = "Result<ProtocolMsg, SendError>")]
pub struct ProtocolMsgWrapper(ProtocolMsg);

impl ProtocolMsgWrapper {
    #[inline]
    fn unwrap(self) -> ProtocolMsg {
        self.0
    }
}

pub trait Wrap {
    fn wrap(self) -> ProtocolMsgWrapper;
}

impl Wrap for ProtocolMsg {
    #[inline]
    fn wrap(self) -> ProtocolMsgWrapper {
        ProtocolMsgWrapper(self)
    }
}

impl Handler<ProtocolMsgWrapper> for Connection {
    type Result = ResponseFuture<Result<ProtocolMsg, SendError>>;

    #[inline]
    fn handle(&mut self, msg: ProtocolMsgWrapper, _ctx: &mut Context<Self>) -> Self::Result {
        Box::pin(Rc::clone(&self.inner).send(msg.unwrap()))
    }
}

#[derive(Debug, ActixMessage)]
#[rtype(result = "()")]
pub struct StopMsg;

impl Handler<StopMsg> for Connection {
    type Result = ();

    fn handle(&mut self, _msg: StopMsg, ctx: &mut Context<Self>) -> Self::Result {
        log::info!("Received StopMsg: actor: {}<{}>", &self.inner.url, &self.inner.id);
        ctx.stop();
    }
}

#[derive(Debug, ActixMessage, Clone)]
#[rtype(result = "()")]
pub enum ConnectionStatusChangedMsg {
    Connected,
    Disconnected,
}

#[derive(Debug, ActixMessage)]
#[rtype(result = "()")]
pub struct SubscribeConnectionStatusMsg(Recipient<ConnectionStatusChangedMsg>);

impl Handler<SubscribeConnectionStatusMsg> for Connection {
    type Result = ();

    fn handle(
        &mut self, msg: SubscribeConnectionStatusMsg, _ctx: &mut Context<Self>,
    ) -> Self::Result {
        self.inner.subscribe(msg.0);
    }
}

#[derive(Debug, ActixMessage)]
#[rtype(result = "()")]
pub struct UnsubscribeConnectionStatusMsg(Recipient<ConnectionStatusChangedMsg>);

impl Handler<UnsubscribeConnectionStatusMsg> for Connection {
    type Result = ();

    fn handle(
        &mut self, msg: UnsubscribeConnectionStatusMsg, _ctx: &mut Context<Self>,
    ) -> Self::Result {
        self.inner.unsubscribe(msg.0);
    }
}

pub trait TimeoutExt {
    type Result;

    fn timeout_ext(self, dur: Duration) -> Self::Result;
}

impl TimeoutExt for Request<Connection, ProtocolMsgWrapper> {
    type Result = Pin<Box<dyn Future<Output = Result<ProtocolMsg, SendError>> + Send>>;

    fn timeout_ext(self, dur: Duration) -> Self::Result {
        Box::pin(async move {
            let (abort_handle, abort_registration) = AbortHandle::new_pair();
            let res = timeout(dur, Abortable::new(self, abort_registration)).await;
            match res {
                Ok(res) => match res {
                    Ok(res) => match res {
                        Ok(res) => match res {
                            Ok(res) => Ok(res),
                            Err(err) => Err(err),
                        },
                        Err(_err) => Err(SendError::MailboxClosed),
                    },
                    Err(_err) => Err(SendError::Timeout),
                },
                Err(_err) => {
                    abort_handle.abort();
                    Err(SendError::Timeout)
                }
            }
        })
    }
}

////////////////////////////////////////////////////////////////////////////////
/// test cases
////////////////////////////////////////////////////////////////////////////////
#[cfg(test)]
mod tests {
    use crate::connection::{Connection, TimeoutExt, Wrap};
    use actix::prelude::*;
    use maxwell_protocol::IntoProtocol;
    use std::time::Duration;

    #[actix::test]
    async fn test_send_msg() {
        log4rs::init_file("config/log4rs.yaml", Default::default()).unwrap();
        let conn = Connection::new(String::from("localhost:8081")).start();
        for _ in 1..10000 {
            let msg = maxwell_protocol::PingReq { r#ref: 1 }.into_protocol().wrap();
            let res = conn.send(msg).timeout_ext(Duration::from_millis(1000)).await;
            log::debug!("result: {:?}", res);
        }
    }
}
