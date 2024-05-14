// Copyright (c) 2024 -  Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::marker::PhantomData;
use std::sync::{Arc, Weak};

use dashmap::DashMap;
use futures::stream::BoxStream;
use futures::StreamExt;
use restate_core::{cancellation_watcher, ShutdownError};
use restate_node_protocol::codec::{Targeted, WireDecode, WireEncode};
use restate_types::NodeId;
use tokio::sync::oneshot;

use restate_core::network::{
    MessageHandler, MessageRouterBuilder, NetworkSendError, NetworkSender,
};
use restate_node_protocol::{MessageEnvelope, RpcMessage, RpcRequest};

use crate::Networking;

/// A router for sending and receiving RPC messages through Networking
///
/// It's responsible for keeping track of in-flight requests, correlating responses, and dropping
/// tracking tokens if caller dropped the future.
pub struct RpcRouter<T>
where
    T: RpcRequest,
{
    networking: Networking,
    response_tracker: ResponseTracker<T>,
}

#[derive(thiserror::Error, Debug)]
#[error(transparent)]
pub enum RpcError {
    #[error("correlation id {0} is already in-flight")]
    CorrelationIdExists(String),
    SendError(#[from] NetworkSendError),
    Shutdown(#[from] ShutdownError),
}

impl<T> RpcRouter<T>
where
    T: RpcRequest + WireEncode + Send + Sync + 'static,
    T::Response: WireDecode + Send + Sync + 'static,
    <T::Response as RpcMessage>::CorrelationId: Send + Sync + From<T::CorrelationId>,
{
    pub fn new(networking: Networking, router_builder: &mut MessageRouterBuilder) -> Self {
        let response_tracker = ResponseTracker::<T>::default();
        router_builder.add_message_handler(response_tracker.clone());
        Self {
            networking,
            response_tracker,
        }
    }

    pub async fn call(&self, to: NodeId, msg: &T) -> Result<MessageEnvelope<T::Response>, RpcError>
    where
        <T::Response as RpcMessage>::CorrelationId: Default,
    {
        let token = self
            .response_tracker
            .new_token(msg.correlation_id().into())
            .ok_or_else(|| RpcError::CorrelationIdExists(format!("{:?}", msg.correlation_id())))?;
        self.networking.send(to, &msg).await?;
        token
            .recv()
            .await
            .map_err(|_| RpcError::Shutdown(ShutdownError))
    }
}

/// A tracker for responses but can be used to track responses for requests that were dispatched
/// via other mechanisms (e.g. ingress flow)
pub struct ResponseTracker<T>
where
    T: RpcRequest,
{
    inner: Arc<Inner<T>>,
}

impl<T> Clone for ResponseTracker<T>
where
    T: RpcRequest,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

struct Inner<T>
where
    T: RpcRequest,
{
    in_flight: DashMap<<T::Response as RpcMessage>::CorrelationId, RpcTokenSender<T::Response>>,
    _phantom: std::marker::PhantomData<T>,
}

impl<T> Default for ResponseTracker<T>
where
    T: RpcRequest,
{
    fn default() -> Self {
        Self {
            inner: Arc::new(Inner {
                in_flight: Default::default(),
                _phantom: PhantomData,
            }),
        }
    }
}

impl<T> ResponseTracker<T>
where
    T: RpcRequest,
{
    pub fn num_in_flight(&self) -> usize {
        self.inner.in_flight.len()
    }

    /// Returns None if an in-flight request holds the same correlation_id.
    pub fn new_token(
        &self,
        correlation_id: <T::Response as RpcMessage>::CorrelationId,
    ) -> Option<RpcToken<T>> {
        let (sender, receiver) = oneshot::channel();
        let existing = self
            .inner
            .in_flight
            .insert(correlation_id.clone(), RpcTokenSender { sender });

        if existing.is_some() {
            return None;
        }
        Some(RpcToken {
            correlation_id,
            router: Arc::downgrade(&self.inner),
            receiver: Some(receiver),
        })
    }

    /// Handle a message through this response tracker. If no request in flight for this message,
    /// the message will be returned.
    pub fn handle_message(
        &self,
        msg: MessageEnvelope<T::Response>,
    ) -> Option<MessageEnvelope<T::Response>> {
        // find the token and send, message is dropped on the floor if no valid match exist for the
        // correlation id.
        if let Some((_, token)) = self.inner.in_flight.remove(&msg.correlation_id()) {
            let _ = token.sender.send(msg);
            None
        } else {
            Some(msg)
        }
    }
}

impl<T> ResponseTracker<T>
where
    T: RpcRequest,
    <T::Response as RpcMessage>::CorrelationId: Default,
{
    /// Returns None if an in-flight request holds the same correlation_id.
    pub fn generate_token(&self) -> Option<RpcToken<T>> {
        let correlation_id = <T::Response as RpcMessage>::CorrelationId::default();
        self.new_token(correlation_id)
    }
}

pub struct StreamingResponseTracker<T>
where
    T: RpcRequest,
    T::Response: WireDecode + Targeted,
{
    flight_tracker: ResponseTracker<T>,
    incoming_messages: BoxStream<'static, MessageEnvelope<T::Response>>,
}

impl<T> StreamingResponseTracker<T>
where
    T: RpcRequest,
    T::Response: WireDecode + Targeted,
{
    pub fn new(incoming_messages: BoxStream<'static, MessageEnvelope<T::Response>>) -> Self {
        let flight_tracker = ResponseTracker::default();
        Self {
            flight_tracker,
            incoming_messages,
        }
    }

    /// Returns None if an in-flight request holds the same correlation_id.
    pub fn new_token(
        &self,
        correlation_id: <T::Response as RpcMessage>::CorrelationId,
    ) -> Option<RpcToken<T>> {
        self.flight_tracker.new_token(correlation_id)
    }

    /// Returns None if an in-flight request holds the same correlation_id.
    pub fn generate_token(&self) -> Option<RpcToken<T>>
    where
        <T::Response as RpcMessage>::CorrelationId: Default,
    {
        let correlation_id = <T::Response as RpcMessage>::CorrelationId::default();
        self.new_token(correlation_id)
    }

    /// Handles the next message. This will **return** the message if no correlated request is
    /// in-flight. otherwise, it's handled by the corresponding token receiver.
    pub async fn handle_next_or_get(&mut self) -> Option<MessageEnvelope<T::Response>> {
        tokio::select! {
            Some(message) = self.incoming_messages.next() => {
                self.flight_tracker.handle_message(message)
            },
            _ = cancellation_watcher() => { None },
            else => { None } ,
        }
    }
}

struct RpcTokenSender<T> {
    sender: oneshot::Sender<MessageEnvelope<T>>,
}

pub struct RpcToken<T>
where
    T: RpcRequest,
{
    correlation_id: <T::Response as RpcMessage>::CorrelationId,
    router: Weak<Inner<T>>,
    // This is Option to get around Rust's borrow checker rules when a type implements the Drop
    // trait. Without this, we cannot move receiver out.
    receiver: Option<oneshot::Receiver<MessageEnvelope<T::Response>>>,
}

impl<T> RpcToken<T>
where
    T: RpcRequest,
{
    pub fn correlation_id(&self) -> <T::Response as RpcMessage>::CorrelationId {
        self.correlation_id.clone()
    }

    /// Awaits the response to come for the associated request. Cancellation safe.
    pub async fn recv(mut self) -> Result<MessageEnvelope<T::Response>, ShutdownError> {
        let receiver = std::mem::take(&mut self.receiver);
        let res = match receiver {
            Some(receiver) => {
                tokio::select! {
                    _ = cancellation_watcher() => {
                        return Err(ShutdownError);
                    },
                    res = receiver => {
                        res.map_err(|_| ShutdownError)
                    }
                }
            }
            // Should never happen unless token was created with None which shouldn't be possible
            None => Err(ShutdownError),
        };
        // If we have received something, we don't need to run drop() since the flight tracker has
        // already removed the sender token.
        std::mem::forget(self);
        res
    }
}

impl<T> Drop for RpcToken<T>
where
    T: RpcRequest,
{
    fn drop(&mut self) {
        // if the router is gone, we can't do anything.
        let Some(router) = self.router.upgrade() else {
            return;
        };
        let _ = router.in_flight.remove(&self.correlation_id);
    }
}

impl<T> MessageHandler for ResponseTracker<T>
where
    T: RpcRequest,
    T::Response: WireDecode + Targeted,
{
    type MessageType = T::Response;

    fn on_message(
        &self,
        msg: restate_node_protocol::MessageEnvelope<Self::MessageType>,
    ) -> impl std::future::Future<Output = ()> + Send {
        self.handle_message(msg);
        std::future::ready(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use restate_node_protocol::common::TargetName;
    use restate_types::GenerationalNodeId;

    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    struct TestCorrelationId(u64);
    struct TestRequest {
        correlation_id: TestCorrelationId,
    }

    impl RpcMessage for TestRequest {
        type CorrelationId = TestCorrelationId;
        fn correlation_id(&self) -> Self::CorrelationId {
            self.correlation_id.clone()
        }
    }

    impl RpcRequest for TestRequest {
        type Response = TestResponse;
    }

    impl Targeted for TestRequest {
        const TARGET: TargetName = TargetName::Unknown;
        fn kind(&self) -> &'static str {
            "TestRequest"
        }
    }

    #[derive(Debug, Clone)]
    struct TestResponse {
        correlation_id: TestCorrelationId,
        text: String,
    }

    impl RpcMessage for TestResponse {
        type CorrelationId = TestCorrelationId;
        fn correlation_id(&self) -> Self::CorrelationId {
            self.correlation_id.clone()
        }
    }

    impl Targeted for TestResponse {
        const TARGET: TargetName = TargetName::Unknown;
        fn kind(&self) -> &'static str {
            "TestMessage"
        }
    }

    impl WireDecode for TestResponse {
        fn decode<B: bytes::Buf>(
            _: &mut B,
            _: restate_node_protocol::common::ProtocolVersion,
        ) -> Result<Self, restate_node_protocol::CodecError>
        where
            Self: Sized,
        {
            unimplemented!()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn test_rpc_flight_tracker_drop() {
        let tracker = ResponseTracker::<TestRequest>::default();
        assert_eq!(tracker.num_in_flight(), 0);
        let token = tracker.new_token(TestCorrelationId(1)).unwrap();
        assert_eq!(tracker.num_in_flight(), 1);
        drop(token);
        assert_eq!(tracker.num_in_flight(), 0);

        let token = tracker.new_token(TestCorrelationId(1)).unwrap();
        assert_eq!(tracker.num_in_flight(), 1);
        // receive with timeout, this should drop the token
        let start = tokio::time::Instant::now();
        let dur = std::time::Duration::from_millis(500);
        let res = tokio::time::timeout(dur, token.recv()).await;
        assert!(res.is_err());
        assert!(start.elapsed() >= dur);
        assert_eq!(tracker.num_in_flight(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn test_rpc_flight_tracker_send_recv() {
        let tracker = ResponseTracker::<TestRequest>::default();
        assert_eq!(tracker.num_in_flight(), 0);
        let token = tracker.new_token(TestCorrelationId(1)).unwrap();
        assert_eq!(tracker.num_in_flight(), 1);

        // dropped on the floor
        tracker
            .on_message(MessageEnvelope::new(
                GenerationalNodeId::new(1, 1),
                22,
                TestResponse {
                    correlation_id: TestCorrelationId(42),
                    text: "test".to_string(),
                },
            ))
            .await;

        assert_eq!(tracker.num_in_flight(), 1);

        let maybe_msg = tracker.handle_message(MessageEnvelope::new(
            GenerationalNodeId::new(1, 1),
            22,
            TestResponse {
                correlation_id: TestCorrelationId(42),
                text: "test".to_string(),
            },
        ));
        assert!(maybe_msg.is_some());

        assert_eq!(tracker.num_in_flight(), 1);

        // matches correlation id
        tracker
            .on_message(MessageEnvelope::new(
                GenerationalNodeId::new(1, 1),
                22,
                TestResponse {
                    correlation_id: TestCorrelationId(1),
                    text: "a very real message".to_string(),
                },
            ))
            .await;

        // sender token is dropped
        assert_eq!(tracker.num_in_flight(), 0);

        let msg = token.recv().await.unwrap();
        assert_eq!(TestCorrelationId(1), msg.correlation_id());
        let (from, msg) = msg.split();
        assert_eq!(GenerationalNodeId::new(1, 1), from);
        assert_eq!("a very real message", msg.text);
    }
}
