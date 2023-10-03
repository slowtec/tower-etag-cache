use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use pin_project::pin_project;
use tokio::sync::oneshot;
use tower_service::Service;

use super::{
    err::ConstLruProviderError, ConstLruProviderCacheKey, ConstLruProviderHandle,
    ConstLruProviderReq, ConstLruProviderRes, ConstLruProviderTResBody, ReqTup,
};

#[pin_project]
pub struct ConstLruProviderPutFuture<ReqBody> {
    #[pin]
    resp_rx: oneshot::Receiver<Result<ConstLruProviderRes<ReqBody>, ConstLruProviderError>>,
}

impl<ReqBody> Future for ConstLruProviderPutFuture<ReqBody> {
    type Output = Result<http::Response<ConstLruProviderTResBody>, ConstLruProviderError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().resp_rx.poll(cx).map(|oneshot_result| {
            oneshot_result.map_or_else(
                |e| Err(ConstLruProviderError::OneshotRecv(e)),
                |result| {
                    result.map(|en| match en {
                        ConstLruProviderRes::Put(r) => r,
                        _ => unreachable!(),
                    })
                },
            )
        })
    }
}

impl<ReqBody, ResBody> Service<(ConstLruProviderCacheKey, http::Response<ResBody>)>
    for ConstLruProviderHandle<ReqBody, ResBody>
where
    ReqTup<ReqBody, ResBody>: Send,
{
    type Response = http::Response<ConstLruProviderTResBody>;

    type Error = ConstLruProviderError;

    type Future = ConstLruProviderPutFuture<ReqBody>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.req_tx
            .poll_reserve(cx)
            .map_err(|_| ConstLruProviderError::MpscSend)
    }

    fn call(
        &mut self,
        (key, resp): (ConstLruProviderCacheKey, http::Response<ResBody>),
    ) -> Self::Future {
        let (resp_tx, resp_rx) = oneshot::channel();
        // safe to ignore err since resp_tx will be dropped
        // here and next poll will fail
        let _ = self
            .req_tx
            .send_item((ConstLruProviderReq::Put(key, resp), resp_tx));
        ConstLruProviderPutFuture { resp_rx }
    }
}
