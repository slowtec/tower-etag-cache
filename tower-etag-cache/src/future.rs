#![allow(clippy::type_complexity)]

use pin_project::pin_project;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tower_service::Service;

use crate::{
    cache_provider::{CacheProvider, CachePutProvider},
    CacheGetResponse, CacheGetResponseResult, EtagCacheRespBody, EtagCacheServiceError,
};

#[pin_project]
pub struct EtagCacheServiceFuture<
    ReqBody,
    ResBody,
    C: CacheProvider<http::Request<ReqBody>, http::Response<ResBody>>,
    S: Service<http::Request<ReqBody>, Response = http::Response<ResBody>>,
> {
    cache_provider: C,
    inner: S,
    #[pin]
    state: EtagCacheServiceFutureState<ReqBody, ResBody, C, S>,
}

impl<
        ReqBody,
        ResBody,
        C: CacheProvider<http::Request<ReqBody>, http::Response<ResBody>>,
        S: Service<http::Request<ReqBody>, Response = http::Response<ResBody>>,
    > EtagCacheServiceFuture<ReqBody, ResBody, C, S>
{
    pub fn cache_get_before(cache_provider: C, inner: S, req: http::Request<ReqBody>) -> Self {
        Self {
            cache_provider,
            inner,
            state: EtagCacheServiceFutureState::CacheGetBefore { req: Some(req) },
        }
    }
}

// using options just to take() and move fields to next state easily
#[pin_project(project = EtagCacheServiceFutureStateProj)]
pub enum EtagCacheServiceFutureState<
    ReqBody,
    ResBody,
    C: CacheProvider<http::Request<ReqBody>, http::Response<ResBody>>,
    S: Service<http::Request<ReqBody>, Response = http::Response<ResBody>>,
> {
    CacheGetBefore {
        req: Option<http::Request<ReqBody>>,
    },
    CacheGet {
        #[pin]
        fut: <C as Service<http::Request<ReqBody>>>::Future,
    },
    InnerBefore {
        key: Option<C::K>,
        req: Option<http::Request<ReqBody>>,
    },
    Inner {
        key: Option<C::K>,
        #[pin]
        fut: S::Future,
    },
    CachePutBefore {
        key: Option<C::K>,
        resp: Option<http::Response<ResBody>>,
    },
    CachePut {
        #[pin]
        fut: <C as Service<(
            <C as CachePutProvider<http::Response<ResBody>>>::Key,
            http::Response<ResBody>,
        )>>::Future,
    },
}

impl<
        ReqBody,
        ResBody,
        C: CacheProvider<http::Request<ReqBody>, http::Response<ResBody>>,
        S: Service<http::Request<ReqBody>, Response = http::Response<ResBody>>,
    > Future for EtagCacheServiceFuture<ReqBody, ResBody, C, S>
{
    type Output = Result<
        http::Response<EtagCacheRespBody<ResBody>>,
        EtagCacheServiceError<
            <C as Service<http::Request<ReqBody>>>::Error,
            S::Error,
            <C as Service<(
                <C as CachePutProvider<http::Response<ResBody>>>::Key,
                http::Response<ResBody>,
            )>>::Error,
        >,
    >;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let mut curr_state = this.state;

        match curr_state.as_mut().project() {
            EtagCacheServiceFutureStateProj::CacheGetBefore { req } => {
                match <C as Service<http::Request<ReqBody>>>::poll_ready(this.cache_provider, cx) {
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(result) => {
                        if let Err(e) = result {
                            return Poll::Ready(Err(EtagCacheServiceError::CacheGetError(e)));
                        }
                        let fut = <C as Service<http::Request<ReqBody>>>::call(
                            this.cache_provider,
                            req.take().unwrap(),
                        );
                        curr_state.set(EtagCacheServiceFutureState::CacheGet { fut });
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                }
            }
            EtagCacheServiceFutureStateProj::CacheGet { fut } => match fut.poll(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(result) => {
                    let CacheGetResponse { req, result } = match result {
                        Ok(r) => r,
                        Err(e) => return Poll::Ready(Err(EtagCacheServiceError::CacheGetError(e))),
                    };
                    let key = match result {
                        CacheGetResponseResult::Hit(headers) => {
                            return Poll::Ready(
                                EtagCacheRespBody::hit_resp(headers)
                                    .map_err(EtagCacheServiceError::ResponseError),
                            );
                        }
                        CacheGetResponseResult::Miss(k) => k,
                    };
                    curr_state.set(EtagCacheServiceFutureState::InnerBefore {
                        key: Some(key),
                        req: Some(req),
                    });
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            },
            EtagCacheServiceFutureStateProj::InnerBefore { key, req } => {
                match this.inner.poll_ready(cx) {
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(result) => {
                        if let Err(e) = result {
                            return Poll::Ready(Err(EtagCacheServiceError::InnerError(e)));
                        }
                        let k = key.take();
                        let fut = this.inner.call(req.take().unwrap());
                        curr_state.set(EtagCacheServiceFutureState::Inner { fut, key: k });
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                }
            }
            EtagCacheServiceFutureStateProj::Inner { key, fut } => match fut.poll(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(result) => {
                    let resp = match result {
                        Ok(r) => r,
                        Err(e) => return Poll::Ready(Err(EtagCacheServiceError::InnerError(e))),
                    };
                    let k = key.take();
                    curr_state.set(EtagCacheServiceFutureState::CachePutBefore {
                        key: k,
                        resp: Some(resp),
                    });
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            },
            EtagCacheServiceFutureStateProj::CachePutBefore { key, resp } => {
                match <C as Service<(
                    <C as CachePutProvider<http::Response<ResBody>>>::Key,
                    http::Response<ResBody>,
                )>>::poll_ready(this.cache_provider, cx)
                {
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(result) => {
                        if let Err(e) = result {
                            return Poll::Ready(Err(EtagCacheServiceError::CachePutError(e)));
                        }
                        let fut = <C as Service<(
                            <C as CachePutProvider<http::Response<ResBody>>>::Key,
                            http::Response<ResBody>,
                        )>>::call(
                            this.cache_provider,
                            (key.take().unwrap(), resp.take().unwrap()),
                        );
                        curr_state.set(EtagCacheServiceFutureState::CachePut { fut });
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                }
            }
            EtagCacheServiceFutureStateProj::CachePut { fut } => match fut.poll(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(result) => Poll::Ready(
                    result
                        .map(EtagCacheRespBody::miss_resp)
                        .map_err(EtagCacheServiceError::CachePutError),
                ),
            },
        }
    }
}
