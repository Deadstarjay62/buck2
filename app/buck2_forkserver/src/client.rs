/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::process::Child;
use std::sync::Arc;

use allocative::Allocative;
use anyhow::Context;
use dupe::Dupe;
use futures::future;
use futures::future::Future;
use futures::future::FutureExt;
use futures::stream;
use futures::stream::StreamExt;
use tonic::transport::Channel;
use tonic::Request;

use crate::convert::decode_event_stream;
use crate::run::decode_command_event_stream;
use crate::run::GatherOutputStatus;

#[derive(Clone, Dupe, Allocative)]
pub struct ForkserverClient {
    inner: Arc<ForkserverClientInner>,
}

#[derive(Allocative)]
struct ForkserverClientInner {
    /// Keep the process reference to prevent its termination.
    #[allocative(skip)]
    _child: Child,
    #[allocative(skip)]
    rpc: buck2_forkserver_proto::forkserver_client::ForkserverClient<Channel>,
}

impl ForkserverClient {
    #[allow(unused)] // Unused on Windows
    pub(crate) fn new(child: Child, channel: Channel) -> Self {
        let rpc = buck2_forkserver_proto::forkserver_client::ForkserverClient::new(channel)
            .max_encoding_message_size(usize::MAX)
            .max_decoding_message_size(usize::MAX);
        Self {
            inner: Arc::new(ForkserverClientInner { _child: child, rpc }),
        }
    }

    pub async fn execute<C>(
        &self,
        req: buck2_forkserver_proto::CommandRequest,
        cancel: C,
    ) -> anyhow::Result<(GatherOutputStatus, Vec<u8>, Vec<u8>)>
    where
        C: Future<Output = ()> + Send + 'static,
    {
        let stream = stream::once(future::ready(buck2_forkserver_proto::RequestEvent {
            data: Some(req.into()),
        }))
        .chain(stream::once(cancel.map(|()| {
            buck2_forkserver_proto::RequestEvent {
                data: Some(buck2_forkserver_proto::CancelRequest {}.into()),
            }
        })));

        let stream = self
            .inner
            .rpc
            .clone()
            .run(stream)
            .await
            .context("Error dispatching command to Forkserver")?
            .into_inner();
        let stream = decode_event_stream(stream);
        decode_command_event_stream(stream).await
    }

    pub async fn set_log_filter(&self, log_filter: String) -> anyhow::Result<()> {
        self.inner
            .rpc
            .clone()
            .set_log_filter(Request::new(buck2_forkserver_proto::SetLogFilterRequest {
                log_filter,
            }))
            .await?;

        Ok(())
    }
}
